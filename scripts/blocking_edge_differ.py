#!/usr/bin/env python3
"""blocking_edge_differ.py — second stateful pass: blocking-command error
precedence + CLIENT UNBLOCK + edge semantics vs redis 7.2.4.

Covers: WRONGTYPE precedence (immediate error vs block), invalid/negative
timeouts, BLMOVE/BRPOPLPUSH src==dst rotation, "DEL does not unblock", blocked
pop served by a MULTI/EXEC push, CLIENT UNBLOCK (TIMEOUT + ERROR forms), BLMPOP
syntax errors, WAIT with no replicas, and BLPOP fed where a wrong-type key sits
behind a good key. Deferred wakes also cover all eight list/zset blocking
commands under narrow ACLs, permission revocation, and multi-key WRONGTYPE
precedence. Use --wake-only for those focused original-command checks.
"""
import argparse
import socket
import sys
import threading
import time


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(5.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError("server closed while reading a RESP line")
            self.b += chunk
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def _rn(self, n):
        while len(self.b) < n + 2:
            chunk = self.s.recv(65536)
            if not chunk:
                raise EOFError("server closed while reading a RESP bulk string")
            self.b += chunk
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t in (b"+", b":", b",", b"#", b"("):
            return l.decode("latin1")
        if t == b"-":
            return "ERR:" + r.decode("latin1")
        if t in (b"$", b"="):
            n = int(r)
            return None if n < 0 else self._rn(n).decode("latin1")
        if t in (b"*", b"~", b">"):
            n = int(r)
            return None if n < 0 else [self.parse() for _ in range(n)]
        if t == b"%":
            n = int(r)
            return ["MAP"] + [self.parse() for _ in range(2 * n)]
        if t == b"_":
            return None
        raise ValueError(l)

    def send(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)

    def cmd(self, *a):
        self.send(*a)
        return self.parse()


def run_original_command_wakes(port):
    """Validate explicit outcomes as well as candidate/oracle equality.

    No fixed feed delay: CLIENT LIST proves the specific waiter has blocked
    before permissions or key types change. Each case checks the queued PING
    and retained data, so matching errors or timeouts cannot pass vacuously.
    """
    observations = {}
    admin = Conn(port)
    username = "blocked_wake_probe"

    def require(actual, expected, label):
        if actual != expected:
            raise AssertionError(f"{label}: expected {expected!r}, got {actual!r}")

    def observe(label, actual, expected):
        require(actual, expected, label)
        observations[label] = actual

    def wait_until_blocked(client_id):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            details = admin.cmd("CLIENT", "LIST", "ID", client_id)
            if not isinstance(details, str) or details.startswith("ERR:"):
                raise AssertionError(f"CLIENT LIST failed: {details!r}")
            fields = dict(field.split("=", 1) for field in details.split() if "=" in field)
            if "b" in fields.get("flags", ""):
                return
            time.sleep(0.005)
        raise AssertionError(f"client {client_id} never entered blocked state")

    cases = [
        (("BLPOP", "q", "0"), "LPOP", False, ["q", "value"]),
        (("BRPOP", "q", "0"), "RPOP", False, ["q", "value"]),
        (("BLMOVE", "q", "dest", "LEFT", "RIGHT", "0"), "LMOVE", False, "value"),
        (("BRPOPLPUSH", "q", "dest", "0"), "LMOVE", False, "value"),
        (("BLMPOP", "0", "1", "q", "LEFT"), "LMPOP", False, ["q", ["value"]]),
        (("BZPOPMIN", "q", "0"), "ZPOPMIN", True, ["q", "value", "1"]),
        (("BZPOPMAX", "q", "0"), "ZPOPMAX", True, ["q", "value", "1"]),
        (("BZMPOP", "0", "1", "q", "MIN"), "ZMPOP", True, ["q", [["value", "1"]]]),
    ]
    try:
        for argv, substitute, zset, success in cases:
            for mode in ("grant_only", "command_revoked", "keys_revoked"):
                label = f"wake/{argv[0].lower()}/{mode}"
                require(admin.cmd("FLUSHALL"), "+OK", label)
                rules = ["reset", "on", "nopass", "~*", "+ping", "+" + argv[0].lower()]
                if mode == "command_revoked":
                    rules.append("+" + substitute.lower())
                require(admin.cmd("ACL", "SETUSER", username, *rules), "+OK", label)
                waiter = Conn(port)
                try:
                    client_id = waiter.cmd("CLIENT", "ID").removeprefix(":")
                    require(waiter.cmd("AUTH", username, ""), "+OK", label)
                    waiter.send(*argv)
                    waiter.send("PING")
                    wait_until_blocked(client_id)
                    expected = success
                    if mode == "command_revoked":
                        require(admin.cmd("ACL", "SETUSER", username, "-" + argv[0].lower()), "+OK", label)
                        expected = f"ERR:NOPERM User {username} has no permissions to run the '{argv[0].lower()}' command"
                    elif mode == "keys_revoked":
                        require(admin.cmd("ACL", "SETUSER", username, "resetkeys", "~other"), "+OK", label)
                        expected = "ERR:NOPERM No permissions to access a key"
                    producer = ("ZADD", "q", "1", "value") if zset else ("LPUSH", "q", "value")
                    require(admin.cmd(*producer), ":1", label)
                    observe(label + "/reply", waiter.parse(), expected)
                    observe(label + "/pipeline", waiter.parse(), "+PONG")
                    observe(label + "/remaining", admin.cmd("ZCARD" if zset else "LLEN", "q"),
                            ":0" if mode == "grant_only" else ":1")
                finally:
                    waiter.s.close()

        multi_key_cases = [
            (("BLPOP", "first", "second", "0"), False),
            (("BRPOP", "first", "second", "0"), False),
            (("BLMPOP", "0", "2", "first", "second", "LEFT"), False),
            (("BZPOPMIN", "first", "second", "0"), True),
            (("BZPOPMAX", "first", "second", "0"), True),
            (("BZMPOP", "0", "2", "first", "second", "MIN"), True),
        ]
        for argv, zset in multi_key_cases:
            label = f"wake/{argv[0].lower()}/earlier_wrongtype"
            require(admin.cmd("FLUSHALL"), "+OK", label)
            waiter = Conn(port)
            try:
                client_id = waiter.cmd("CLIENT", "ID").removeprefix(":")
                waiter.send(*argv)
                waiter.send("PING")
                wait_until_blocked(client_id)
                require(admin.cmd("MULTI"), "+OK", label)
                require(admin.cmd("SET", "first", "wrong-type"), "+QUEUED", label)
                producer = ("ZADD", "second", "1", "value") if zset else ("LPUSH", "second", "value")
                require(admin.cmd(*producer), "+QUEUED", label)
                require(admin.cmd("EXEC"), ["+OK", ":1"], label)
                observe(label + "/reply", waiter.parse(),
                        "ERR:WRONGTYPE Operation against a key holding the wrong kind of value")
                observe(label + "/pipeline", waiter.parse(), "+PONG")
                observe(label + "/remaining", admin.cmd("ZCARD" if zset else "LLEN", "second"), ":1")
            finally:
                waiter.s.close()
        require(admin.cmd("ACL", "DELUSER", username), ":1", "remove probe user")
        return observations
    finally:
        admin.s.close()


def run(port):
    r = {}
    c = Conn(port)
    c.cmd("FLUSHALL")

    # --- WRONGTYPE precedence: BLPOP on a string key returns immediate error ---
    c.cmd("SET", "str", "v")
    r["blpop_wrongtype"] = c.cmd("BLPOP", "str", "0.3")
    r["brpop_wrongtype"] = c.cmd("BRPOP", "str", "0.3")
    r["bzpopmin_wrongtype"] = c.cmd("BZPOPMIN", "str", "0.3")
    r["blmove_wrongtype_src"] = c.cmd("BLMOVE", "str", "d", "LEFT", "RIGHT", "0.3")
    # WRONGTYPE behind an empty key: first key empty, second wrong type -> blocks then times out? redis checks types up front
    c.cmd("DEL", "e1")
    r["blpop_2nd_wrongtype"] = c.cmd("BLPOP", "e1", "str", "0.3")

    # --- invalid timeouts ---
    r["blpop_neg_timeout"] = c.cmd("BLPOP", "k", "-1")
    r["blpop_nan_timeout"] = c.cmd("BLPOP", "k", "abc")
    r["blpop_inf_timeout"] = c.cmd("BLPOP", "k", "inf")
    r["blmpop_bad_numkeys"] = c.cmd("BLMPOP", "0.1", "0", "LEFT")
    r["blmpop_bad_dir"] = c.cmd("BLMPOP", "0.1", "1", "k", "SIDEWAYS")
    r["bzmpop_bad"] = c.cmd("BZMPOP", "0.1", "1", "k", "MIDDLE")

    # --- BLMOVE / BRPOPLPUSH rotate (src == dst) ---
    c.cmd("DEL", "rot")
    c.cmd("RPUSH", "rot", "a", "b", "c")
    r["blmove_rotate"] = c.cmd("BLMOVE", "rot", "rot", "LEFT", "RIGHT", "0.3")
    r["rot_after"] = c.cmd("LRANGE", "rot", "0", "-1")
    c.cmd("DEL", "rot2")
    c.cmd("RPUSH", "rot2", "x", "y", "z")
    r["brpoplpush_rotate"] = c.cmd("BRPOPLPUSH", "rot2", "rot2", "0.3")
    r["rot2_after"] = c.cmd("LRANGE", "rot2", "0", "-1")

    # --- DEL does NOT unblock (waiter should still time out) ---
    def waiter_then(actions, waiter_cmd, delay=0.15, jointime=3):
        res = {}
        w = Conn(port)

        def go():
            try:
                res["r"] = w.cmd(*waiter_cmd)
            except Exception as e:
                res["r"] = ("EXC", str(e))
        th = threading.Thread(target=go)
        th.start()
        time.sleep(delay)
        a = Conn(port)
        for act in actions:
            a.cmd(*act)
        th.join(jointime)
        return res.get("r", ("TIMEOUT",))

    c.cmd("RPUSH", "dz", "seed")
    c.cmd("LPOP", "dz")  # now empty/deleted
    r["del_no_unblock"] = waiter_then([("DEL", "dz"), ("SET", "dz", "notalist")],
                                      ("BLPOP", "dz", "0.6"))

    # --- blocked pop served by a MULTI/EXEC push ---
    r["served_by_multi"] = waiter_then(
        [("MULTI",)],  # placeholder; do the real multi below
        ("BLPOP", "mk", "2"), delay=0.0, jointime=0.05)
    # do it properly: separate feeder runs MULTI/EXEC
    def served_by_multi():
        res = {}
        w = Conn(port)

        def go():
            try:
                res["r"] = w.cmd("BLPOP", "mxk", "2")
            except Exception as e:
                res["r"] = ("EXC", str(e))
        th = threading.Thread(target=go)
        th.start()
        time.sleep(0.15)
        a = Conn(port)
        a.cmd("MULTI")
        a.cmd("RPUSH", "mxk", "viamulti")
        a.cmd("EXEC")
        th.join(3)
        return res.get("r", ("TIMEOUT",))
    r["served_by_multi"] = served_by_multi()

    # --- CLIENT UNBLOCK (TIMEOUT form -> waiter gets nil; returns :1) ---
    def client_unblock(form):
        res = {}
        w = Conn(port)
        wid = w.cmd("CLIENT", "ID")

        def go():
            try:
                res["r"] = w.cmd("BLPOP", "ubk_" + form, "3")
            except Exception as e:
                res["r"] = ("EXC", str(e))
        th = threading.Thread(target=go)
        th.start()
        time.sleep(0.2)
        a = Conn(port)
        if form == "timeout":
            res["unblock_reply"] = a.cmd("CLIENT", "UNBLOCK", wid, "TIMEOUT")
        else:
            res["unblock_reply"] = a.cmd("CLIENT", "UNBLOCK", wid, "ERROR")
        th.join(3)
        return res.get("r", ("TIMEOUT",)), res.get("unblock_reply")
    r["unblock_timeout_reply"], _ = (lambda x: (x[1], None))(client_unblock("timeout"))
    rt = client_unblock("timeout")
    r["unblock_timeout_waiter"] = rt[0]
    r["unblock_timeout_ret"] = rt[1]
    re_ = client_unblock("error")
    r["unblock_error_waiter"] = re_[0]
    r["unblock_error_ret"] = re_[1]
    # CLIENT UNBLOCK on a non-blocked / nonexistent client -> :0
    cc = Conn(port)
    r["unblock_notblocked"] = cc.cmd("CLIENT", "UNBLOCK", "999999999")

    # --- WAIT with no replicas: WAIT 0 100 returns 0 immediately ---
    r["wait_0"] = c.cmd("WAIT", "0", "100")
    r["wait_1_shorttimeout"] = c.cmd("WAIT", "1", "150")  # 0 after ~150ms (no replicas)

    r.update(run_original_command_wakes(port))
    return r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    ap.add_argument("--wake-only", action="store_true",
                    help="run only deferred original-command/ACL wake regressions")
    args = ap.parse_args()

    runner = run_original_command_wakes if args.wake_only else run
    o = runner(args.oracle)
    f = runner(args.fr)
    diffs = 0
    for k in o:
        if o[k] != f.get(k):
            diffs += 1
            print(f"DIFF [{k}]")
            print(f"   oracle: {o[k]!r}")
            print(f"   fr    : {f.get(k)!r}")
    if diffs:
        print(f"\nFAIL: {diffs} blocking-edge divergences")
        sys.exit(1)
    print(f"OK: {len(o)} blocking-edge cases byte-exact vs redis 7.2.4")


if __name__ == "__main__":
    main()
