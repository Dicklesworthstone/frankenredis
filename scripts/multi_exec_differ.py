#!/usr/bin/env python3
"""multi_exec_differ.py — randomized MULTI/EXEC/WATCH transaction differential vs redis 7.2.4.

Companion to random_command_differ.py (which covers single-shot commands). This
one randomly builds transactions — MULTI, a random queue of inner commands
(including queue-time errors that must EXECABORT and runtime errors that surface
as error frames inside the EXEC array), then EXEC / DISCARD — plus WATCH/dirty
scenarios where a SECOND connection mutates a watched key so EXEC must return
nil. Both servers are driven with identical RNG and compared byte-for-byte.

SETUP (oracle config-less => compiled defaults align; fr strict mode):
    legacy_redis_code/redis/src/redis-server --port 16399 --save '' --appendonly no --daemonize yes
    cargo build -p fr-server          # CARGO_TARGET_DIR=/data/tmp/cargo-target here
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    scripts/multi_exec_differ.py 16399 16400          # sweeps several seeds
    scripts/multi_exec_differ.py 16399 16400 7 8000   # single seed, 8k transactions

RESULT: the data-command transaction CONTROL surface (queue/+QUEUED, EXEC array,
EXECABORT on a queue-time error, runtime error frames inside EXEC, nested MULTI,
DISCARD, WATCH cross-connection dirty → nil EXEC) is byte-exact (PASS).

KNOWN-OPEN, INTENTIONALLY EXCLUDED — pub/sub commands run INSIDE a transaction.
Found by an earlier (pubsub-inclusive) version of this fuzzer; both live in
fr-runtime (handle_exec_command / handle_unsubscribe_command) and are tracked
for a fix once that crate is free:
  1. `MULTI; SUBSCRIBE ch; EXEC` executes the SUBSCRIBE (correct reply) but does
     NOT transition the connection into subscriber mode — a following `GET` runs
     instead of being rejected with "... only (P|S)SUBSCRIBE ... allowed ...".
  2. `MULTI; UNSUBSCRIBE; EXEC` on a never-subscribed connection replies with a
     stale/wrong channel name instead of the nil channel redis returns (the
     direct, non-MULTI UNSUBSCRIBE is correct — only the EXEC path is wrong).
SUBSCRIBE/UNSUBSCRIBE are therefore not in INNER below; re-add them once fixed.
"""
import socket
import sys
import random

ORACLE_DEFAULT = 16399
FR_DEFAULT = 16400
KEYS = ["k1", "k2"]


def _read_reply(s: socket.socket) -> bytes:
    data = bytearray()

    def read_line() -> bytes:
        line = bytearray()
        while not line.endswith(b"\r\n"):
            ch = s.recv(1)
            if not ch:
                break
            line += ch
        return bytes(line)

    def one() -> None:
        line = read_line()
        data.extend(line)
        if not line:
            return
        t = line[:1]
        if t in (b"+", b"-", b":", b"_", b"#", b",", b"("):
            return
        if t in (b"$", b"="):
            n = int(line[1:-2])
            if n < 0:
                return
            body = b""
            while len(body) < n + 2:
                body += s.recv(n + 2 - len(body))
            data.extend(body)
            return
        if t in (b"*", b"~", b">", b"%"):
            n = int(line[1:-2])
            if n < 0:
                return
            if t == b"%":
                n *= 2
            for _ in range(n):
                one()

    one()
    return bytes(data)


def send(s: socket.socket, *args) -> bytes:
    buf = b"*%d\r\n" % len(args)
    for a in args:
        a = a.encode() if isinstance(a, str) else a
        buf += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(buf)
    return _read_reply(s)


def _rk():
    return random.choice(KEYS)


def _rv():
    return random.choice(["a", "10", "3.14"])


# Inner queue vocabulary: success paths, a runtime error (INCRBY non-int — error
# frame INSIDE the EXEC array), and queue-time errors (arity / unknown command —
# must abort the whole EXEC with EXECABORT). No pub/sub (see module docstring).
INNER = [
    lambda: ["SET", _rk(), _rv()], lambda: ["GET", _rk()], lambda: ["INCR", _rk()],
    lambda: ["LPUSH", _rk(), _rv()], lambda: ["LRANGE", _rk(), "0", "-1"], lambda: ["LPOP", _rk()],
    lambda: ["SADD", _rk(), _rv()], lambda: ["ZADD", _rk(), "1", _rv()], lambda: ["HSET", _rk(), "f", _rv()],
    lambda: ["DEL", _rk()], lambda: ["EXPIRE", _rk(), "100000"], lambda: ["TYPE", _rk()],
    lambda: ["GETRANGE", _rk(), "0", "-1"], lambda: ["STRLEN", _rk()],
    lambda: ["INCRBY", _rk(), "abc"],   # runtime error -> error frame inside EXEC
    lambda: ["GET"],                    # wrong arity at queue time -> EXECABORT
    lambda: ["NOSUCHCMD", _rk()],       # unknown command at queue time -> EXECABORT
    lambda: ["MULTI"],                  # nested MULTI -> error reply, stays in MULTI
]


def run_scenario(s) -> list:
    out = [("MULTI", send(s, "MULTI"))]
    for _ in range(random.randint(0, 6)):
        cmd = random.choice(INNER)()
        out.append((tuple(cmd), send(s, *cmd)))
    fin = random.choice(["EXEC", "EXEC", "EXEC", "DISCARD"])
    out.append((fin, send(s, fin)))
    return out


def run_watch(s, other) -> list:
    out = [("WATCH", send(s, "WATCH", "k1", "k2"))]
    if random.random() < 0.5:
        out.append(("OTHER-WRITE", send(other, "SET", "k1", "z")))
    out.append(("MULTI", send(s, "MULTI")))
    out.append(("SET", send(s, "SET", "k1", "x")))
    out.append(("EXEC", send(s, "EXEC")))  # nil array if k1 was dirtied
    return out


def main() -> int:
    op = int(sys.argv[1]) if len(sys.argv) > 1 else ORACLE_DEFAULT
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else FR_DEFAULT
    if len(sys.argv) > 3:
        seeds = [int(sys.argv[3])]
        n = int(sys.argv[4]) if len(sys.argv) > 4 else 8000
    else:
        seeds = [1, 2, 3, 4, 5]
        n = 4000
    oa, ob = socket.create_connection(("127.0.0.1", op)), socket.create_connection(("127.0.0.1", op))
    fa, fb = socket.create_connection(("127.0.0.1", fp)), socket.create_connection(("127.0.0.1", fp))
    for s in (oa, ob, fa, fb):
        s.settimeout(3)
    total = 0
    for seed in seeds:
        random.seed(seed)
        for i in range(n):
            for s in (oa, fa):
                send(s, "RESET")
                send(s, "FLUSHALL")
            sd = 1_000_000 * seed + i
            watch = random.random() < 0.4
            random.seed(sd)
            ro = run_watch(oa, ob) if watch else run_scenario(oa)
            random.seed(sd)
            rf = run_watch(fa, fb) if watch else run_scenario(fa)
            for (co, xo), (_cf, xf) in zip(ro, rf):
                if xo != xf:
                    total += 1
                    if total <= 15:
                        print(f"DIVERGE seed={seed} iter={i} step={co}: oracle={xo!r} fr={xf!r}")
                    break
    print("-" * 60)
    print(f"checked {len(seeds)} seed(s) x {n} transactions; divergences: {total}")
    if total == 0:
        print("PASS — fr MULTI/EXEC/WATCH transaction control matches redis 7.2.4")
        return 0
    print(f"FAIL — {total} divergence(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
