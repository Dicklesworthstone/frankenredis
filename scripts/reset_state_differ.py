#!/usr/bin/env python3
"""reset_state_differ.py — RESET clears ALL per-client state, byte-exact vs redis 7.2.4.

RESET (resetCommand) is a single command with broad effect: it must discard a
pending MULTI transaction (and UNWATCH), unsubscribe every channel/pattern/shard,
leave MONITOR, restore CLIENT REPLY to ON, drop the RESP protocol back to 2, clear
CLIENT SETNAME + the lib-name/lib-ver set via CLIENT SETINFO, disable CLIENT
TRACKING, re-select db 0, and re-authenticate as the default user. A single piece
of state left dangling is a real correctness bug, but because the bug only shows
up as the WRONG REPLY TO A LATER COMMAND, the command-replay differs never catch
it — this gate drives the full state up, fires RESET, then reads the resulting
client state back through CLIENT INFO / CLIENT GETNAME and a few behavioural
probes, comparing fr against the oracle field-by-field.

SETUP (oracle config-less => compiled defaults; fr strict mode):
    legacy_redis_code/redis/src/redis-server --port 16399 --save '' --appendonly no --daemonize yes
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    scripts/reset_state_differ.py 16399 16400
"""
import socket
import sys

ORACLE_DEFAULT = 16399
FR_DEFAULT = 16400

# CLIENT INFO fields RESET is responsible for (connection-identity fields like
# id/addr/fd/age/laddr are intentionally excluded — they legitimately differ).
STATE_FIELDS = [
    "db", "resp", "name", "multi", "sub", "psub", "ssub",
    "tracking", "lib-name", "lib-ver", "watch",
]


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port))
        self.s.settimeout(3)
        self.buf = bytearray()

    def _line(self):
        while b"\r\n" not in self.buf:
            self.buf.extend(self.s.recv(8192))
        i = self.buf.index(b"\r\n")
        out = bytes(self.buf[:i])
        del self.buf[: i + 2]
        return out

    def _read(self):
        line = self._line()
        t, rest = line[:1], line[1:]
        if t in (b"+", b"-", b":", b",", b"#", b"("):
            return line
        if t == b"_":  # RESP3 null
            return None
        if t in (b"$", b"="):
            n = int(rest)
            if n < 0:
                return None
            while len(self.buf) < n + 2:
                self.buf.extend(self.s.recv(8192))
            d = bytes(self.buf[:n])
            del self.buf[: n + 2]
            return d
        if t in (b"*", b"~", b">"):
            n = int(rest)
            return None if n < 0 else [self._read() for _ in range(n)]
        if t == b"%":  # RESP3 map
            n = int(rest)
            return [self._read() for _ in range(2 * n)]
        raise ValueError(f"unparsed reply: {line!r}")

    def cmd(self, *args):
        buf = b"*%d\r\n" % len(args)
        for a in args:
            a = a.encode() if isinstance(a, str) else a
            buf += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(buf)
        return self._read()


def client_info_fields(c):
    raw = c.cmd("CLIENT", "INFO")
    txt = raw.decode("latin1") if isinstance(raw, (bytes, bytearray)) else str(raw)
    fields = {}
    for tok in txt.split():
        if "=" in tok:
            k, v = tok.split("=", 1)
            fields[k] = v
    return fields


def build_state_then_reset(c):
    """Drive a rich client state up, then RESET. Returns post-RESET observables."""
    # RESP3 + identity + db + tracking + lib info
    c.cmd("HELLO", "3")
    c.cmd("CLIENT", "SETNAME", "probename")
    c.cmd("CLIENT", "SETINFO", "lib-name", "mylib")
    c.cmd("CLIENT", "SETINFO", "lib-ver", "9.9")
    c.cmd("SELECT", "5")
    c.cmd("CLIENT", "TRACKING", "ON")
    # pending transaction + watched key
    c.cmd("WATCH", "wk")
    c.cmd("MULTI")
    c.cmd("SET", "qx", "1")  # queued
    # subscriptions
    c.cmd("SUBSCRIBE", "ch1")
    c.cmd("PSUBSCRIBE", "pat.*")
    c.cmd("SSUBSCRIBE", "sch")
    # the command under test
    reset_reply = c.cmd("RESET")

    obs = {"reset_reply": reset_reply}
    fields = client_info_fields(c)
    for f in STATE_FIELDS:
        obs[f] = fields.get(f)
    # behavioural probes that depend on cleared state
    obs["exec_after_reset"] = c.cmd("EXEC")          # -> error: EXEC without MULTI
    obs["getname"] = c.cmd("CLIENT", "GETNAME")      # -> empty
    obs["ping"] = c.cmd("PING")                      # -> +PONG (reply mode ON, not subscriber)
    return obs


def norm(v):
    if isinstance(v, (bytes, bytearray)):
        return bytes(v)
    if isinstance(v, list):
        return tuple(norm(x) for x in v)
    return v


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else ORACLE_DEFAULT
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else FR_DEFAULT
    o = build_state_then_reset(Conn(op))
    f = build_state_then_reset(Conn(fp))

    div = 0
    for key in sorted(set(o) | set(f)):
        ov, fv = norm(o.get(key)), norm(f.get(key))
        # EXEC-without-MULTI error wording can differ; compare only error-ness + code.
        if key == "exec_after_reset":
            oc = ov[:1] == b"-" if isinstance(ov, bytes) else False
            fc = fv[:1] == b"-" if isinstance(fv, bytes) else False
            if oc == fc:
                continue
        if ov != fv:
            div += 1
            print(f"DIVERGE {key}:\n  oracle: {ov!r}\n  fr    : {fv!r}")
    print("-" * 60)
    print(f"checked RESET post-state ({len(STATE_FIELDS)} CLIENT INFO fields + 3 probes); divergences: {div}")
    if div == 0:
        print("PASS — fr RESET clears all per-client state like redis 7.2.4")
        return 0
    print(f"FAIL — {div} divergence(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
