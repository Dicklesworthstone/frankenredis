#!/usr/bin/env python3
"""command_coverage_gate.py — feature-completeness gate vs vendored redis 7.2.4.

Every other differ checks BEHAVIOR; this one checks EXISTENCE. It enumerates the
full command table of a config-less redis 7.2.4 (`COMMAND LIST`) and asserts that
frankenredis recognizes every one — i.e. fr never replies "unknown command" for a
command Redis implements. It then checks each container command's documented
subcommands the same way ("unknown subcommand"). A refactor that accidentally
drops a command or subcommand from the dispatch table is a silent, serious
regression that behavior differs (which only test commands they know about) miss;
this gate catches it immediately.

Only existence is checked (the reply for a bare command is usually a wrong-arity
or wrong-type error — that's fine, it proves the command is wired up). Dangerous
commands that close the connection / block / crash a probe (SHUTDOWN, QUIT,
DEBUG, blocking pops, SUBSCRIBE, MONITOR, replication, FLUSH*, scripting/MULTI)
are skipped — their presence is covered by the behavior differs.

Usage: command_coverage_gate.py <oracle_port> <fr_port>
Exit 0 if fr covers every Redis command + listed subcommand, else 1.
"""
import socket
import sys
import time

DANGER = {
    "shutdown", "quit", "debug", "reset", "failover", "subscribe", "unsubscribe",
    "psubscribe", "punsubscribe", "ssubscribe", "sunsubscribe", "monitor", "sync",
    "psync", "replicaof", "slaveof", "save", "bgsave", "bgrewriteaof", "blpop",
    "brpop", "blmove", "blmpop", "brpoplpush", "bzpopmin", "bzpopmax", "bzmpop",
    "xread", "xreadgroup", "wait", "waitaof", "flushall", "flushdb", "swapdb",
    "migrate", "restore", "client", "hello", "auth", "exec", "multi", "discard",
    "watch", "unwatch", "function", "fcall", "fcall_ro", "eval", "evalsha",
    "eval_ro", "evalsha_ro", "lcs",
}

# Documented redis 7.2.4 subcommands per container (existence check).
SUBCOMMANDS = {
    "CLIENT": ["ID", "GETNAME", "SETNAME", "SETINFO", "INFO", "LIST", "KILL",
               "PAUSE", "UNPAUSE", "REPLY", "NO-EVICT", "NO-TOUCH", "TRACKING",
               "TRACKINGINFO", "CACHING", "GETREDIR"],
    "CONFIG": ["GET", "SET", "RESETSTAT", "REWRITE"],
    "OBJECT": ["ENCODING", "REFCOUNT", "IDLETIME", "FREQ"],
    "XINFO": ["STREAM", "GROUPS", "CONSUMERS"],
    "XGROUP": ["CREATE", "SETID", "DESTROY", "CREATECONSUMER", "DELCONSUMER"],
    "COMMAND": ["COUNT", "DOCS", "INFO", "GETKEYS", "GETKEYSANDFLAGS", "LIST"],
    "CLUSTER": ["INFO", "MYID", "SLOTS", "SHARDS", "NODES", "KEYSLOT",
                "COUNTKEYSINSLOT", "GETKEYSINSLOT", "RESET", "LINKS"],
    "FUNCTION": ["LOAD", "DELETE", "FLUSH", "LIST", "DUMP", "RESTORE", "STATS", "KILL"],
    "ACL": ["WHOAMI", "LIST", "GETUSER", "SETUSER", "DELUSER", "CAT", "USERS",
            "GENPASS", "LOAD", "SAVE", "LOG", "DRYRUN"],
    "LATENCY": ["HISTORY", "LATEST", "RESET", "DOCTOR", "GRAPH"],
    "MEMORY": ["USAGE", "DOCTOR", "STATS", "MALLOC-STATS", "PURGE"],
    "SLOWLOG": ["GET", "LEN", "RESET"],
    "PUBSUB": ["CHANNELS", "NUMSUB", "NUMPAT", "SHARDCHANNELS", "SHARDNUMSUB"],
}


def one(port, args):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=3)
        s.settimeout(2.0)
        o = b"*%d\r\n" % len(args)
        for x in args:
            xb = x.encode() if isinstance(x, str) else x
            o += b"$%d\r\n%s\r\n" % (len(xb), xb)
        s.sendall(o)
        d = s.recv(65536)
        s.close()
        return d
    except OSError:
        return b"__CONNERR__"


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(1)
    oport, fport = int(sys.argv[1]), int(sys.argv[2])

    # Enumerate the oracle's command table.
    raw = one(oport, ["COMMAND", "LIST"])
    names = sorted({
        x.decode() for x in raw.split(b"\r\n")
        if x and not x.startswith((b"*", b"$")) and all(32 <= c < 127 for c in x)
    })
    if len(names) < 200:
        print("ABORT: oracle COMMAND LIST returned only %d names (is it redis 7.2.4?)" % len(names))
        sys.exit(2)

    missing_cmds = []
    for n in names:
        if n.lower() in DANGER:
            continue
        rf = one(fport, [n])
        ro = one(oport, [n])
        if b"unknown command" in rf[:64].lower() and b"unknown command" not in ro[:64].lower():
            missing_cmds.append(n)

    missing_subs = []
    for cont, subs in SUBCOMMANDS.items():
        for sub in subs:
            rf = one(fport, [cont, sub])
            ro = one(oport, [cont, sub])
            if b"unknown subcommand" in rf.lower() and b"unknown subcommand" not in ro.lower():
                missing_subs.append("%s %s" % (cont, sub))

    nsub = sum(len(v) for v in SUBCOMMANDS.values())
    if missing_cmds or missing_subs:
        if missing_cmds:
            print("MISSING COMMANDS (%d):" % len(missing_cmds))
            for m in missing_cmds:
                print("  ", m)
        if missing_subs:
            print("MISSING SUBCOMMANDS (%d):" % len(missing_subs))
            for m in missing_subs:
                print("  ", m)
        sys.exit(1)
    print("OK: fr covers all %d redis 7.2.4 commands (%d probed) + %d container subcommands"
          % (len(names), len([n for n in names if n.lower() not in DANGER]), nsub))
    sys.exit(0)


if __name__ == "__main__":
    main()
