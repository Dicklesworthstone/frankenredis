#!/usr/bin/env python3
"""Differential gate: redis.call / redis.pcall command-error semantics (frankenredis-0czgc).

Locks the fix that redis.call RAISES a command-error reply (aborting the script, with
redis's "script: <sha>" context suffix) rather than returning it as a value, plus the
script command-lookup rewrites:
  - an unresolvable command OR container subcommand -> "Unknown Redis command called
    from script" (the verbatim "unknown subcommand '<x>'. Try <CMD> HELP." is DIRECT-only)
  - an arity failure -> "Wrong number of args calling Redis command from script"
  - redis.pcall packages the same error as {err=...} and the script CONTINUES.

These were ~160 Ok(RespFrame::Error) command-error sites that, before the fix, let the
script continue past a failed redis.call and dropped the script suffix.

Usage: lua_rediscall_error_differ.py <oracle_port> <fr_port>   (default 16399 16400)
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1", p), timeout=6)
def cmd(s, *a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    s.sendall(o); time.sleep(0.02); return s.recv(1 << 20)

def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    o, f = conn(op), conn(fp)
    for s in (o, f):
        cmd(s, "FLUSHALL"); cmd(s, "RPUSH", "l", "a"); cmd(s, "SET", "str", "v")
    fails = []
    def chk(label, inner):
        ro, rf = cmd(o, "EVAL", inner, "0"), cmd(f, "EVAL", inner, "0")
        if ro != rf:
            fails.append(f"{label}: redis={ro[:70]!r} fr={rf[:70]!r}")
    # control-flow: redis.call command error ABORTS the script (no 'AFTER')
    for nm, c in [("xadd00", b"redis.call('XADD','st','0-0','f','v')"),
                  ("maxlenneg", b"redis.call('XADD','st','MAXLEN','-1','*','f','v')"),
                  ("incrnonint", b"redis.call('INCR','str')"),
                  ("wrongtype", b"redis.call('LPUSH','str','x')"),
                  ("sortbad", b"redis.call('SORT','l','BOGUS')"),
                  ("lposrank0", b"redis.call('LPOS','l','a','RANK','0')")]:
        chk(f"abort_{nm}", c + b"; return 'AFTER'")
        chk(f"return_{nm}", b"return " + c)
    # bad container subcommand -> "Unknown Redis command called from script"
    for c in [b"OBJECT", b"CLIENT", b"CONFIG", b"XINFO", b"COMMAND", b"CLUSTER",
              b"ACL", b"MEMORY", b"FUNCTION", b"SCRIPT", b"XGROUP", b"LATENCY"]:
        chk(b"badsub_".decode() + c.decode(), b"return redis.call('" + c + b"','BADSUB')")
    # arity -> "Wrong number of args calling Redis command from script"
    for nm, c in [("get", b"GET"), ("set", b"SET','k"), ("aclgetuser", b"ACL','GETUSER")]:
        chk(f"arity_{nm}", b"return redis.call('" + c + b"')")
    # unknown top-level command
    chk("unknown_cmd", b"return redis.call('TOTALLYNOTACOMMAND')")
    # pcall packages {err=...} and CONTINUES
    chk("pcall_continues", b"redis.pcall('XADD','st','0-0','f','v'); return 'AFTER'")
    chk("pcall_err_field", b"local r=redis.pcall('SORT','l','BOGUS'); return r.err")
    chk("pcall_badsub_err", b"local r=redis.pcall('OBJECT','BADSUB'); return r.err")
    # valid commands unaffected
    chk("valid_setget", b"redis.call('SET','k','v1'); return redis.call('GET','k')")
    chk("valid_nested_pcall", b"local ok=pcall(function() return redis.call('INCR','str') end); return tostring(ok)")
    if fails:
        print(f"FAIL — {len(fails)} redis.call/pcall error divergence(s) vs redis 7.2.4:")
        for x in fails[:15]:
            print(f"  {x}")
        sys.exit(1)
    print("PASS — redis.call/pcall command-error semantics byte-exact vs redis 7.2.4 "
          "(raise+abort+script-suffix, unknown-cmd/subcmd & arity rewrites, pcall {err=} table)")

if __name__ == "__main__":
    main()
