#!/usr/bin/env python3
"""frankenredis-o500d: differential FUNCTION LOAD against live Redis 7.2.4.

fr syntax-checks a library body and then TEXT-SCANS for register_function; it never
EXECUTES the body. Upstream functions.c compiles AND RUNS it at load time inside a
sandbox whose global table exposes only the declared set, so a load-time runtime
error — or a read of an undeclared global — fails the load before anything registers.

Both arms run in ONE invocation against two live servers. Usage:
    function_load_differ.py <redis_port> <fr_port>
Exit 0 = arms agree on every row, 1 = at least one divergence.
"""
import socket
import sys

RS = int(sys.argv[1])
FR = int(sys.argv[2])

SHEBANG = "#!lua name=%s\n"


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 10)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""

    def _enc(self, args):
        out = [b"*%d\r\n" % len(args)]
        for a in args:
            if isinstance(a, str):
                a = a.encode()
            out.append(b"$%d\r\n%s\r\n" % (len(a), a))
        return b"".join(out)

    def _line(self):
        while b"\r\n" not in self.buf:
            chunk = self.s.recv(1 << 20)
            if not chunk:
                raise EOFError("server closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag in (b"+", b":"):
            return rest
        if tag == b"-":
            return b"ERRREPLY:" + rest
        if tag == b"$":
            n = int(rest)
            if n == -1:
                return None
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(1 << 20)
            val, self.buf = self.buf[:n], self.buf[n + 2:]
            return val
        if tag == b"*":
            n = int(rest)
            if n == -1:
                return None
            return [self._read() for _ in range(n)]
        raise RuntimeError(f"bad tag {line!r}")

    def cmd(self, *args):
        self.s.sendall(self._enc(list(args)))
        return self._read()


# (body_suffix, description, expectation_class)
CASES = [
    ("top_level_error",
     "error('boom')\nredis.register_function('f', function(k,a) return 1 end)",
     "load-time runtime error"),
    ("undeclared_global",
     "local x = nosuchglobal_zz\nredis.register_function('f', function(k,a) return 1 end)",
     "read of undeclared global"),
    ("tonumber_call",
     "local x = tonumber('1')\nredis.register_function('f', function(k,a) return 1 end)",
     "call of undeclared global tonumber"),
    ("nil_index",
     "local t = nil\nlocal y = t.field\nredis.register_function('f', function(k,a) return 1 end)",
     "nil index at load time"),
    # ---- controls: both arms MUST already agree on these ----
    ("CONTROL_no_register",
     "local x = 1",
     "CONTROL: body registers nothing"),
    ("CONTROL_valid",
     "redis.register_function('f', function(k,a) return 1 end)",
     "CONTROL: valid library"),
]


def classify(reply):
    if reply is None:
        return "nil"
    if isinstance(reply, bytes) and reply.startswith(b"ERRREPLY:"):
        return "ERR " + reply[len(b"ERRREPLY:"):].decode(errors="replace")
    if isinstance(reply, bytes):
        return "OK " + reply.decode(errors="replace")
    return repr(reply)


def main():
    redis, fr = Conn(RS), Conn(FR)
    for c in (redis, fr):
        c.cmd("FUNCTION", "FLUSH")

    divergences = 0
    control_failures = 0
    print(f"{'case':<22} {'fr':<44} {'redis 7.2.4'}")
    print("-" * 118)
    for i, (name, body, _desc) in enumerate(CASES):
        lib = f"o500d{i}"
        # (frankenredis-niu8g) Each case needs its own FUNCTION name, not just
        # its own library name. A function name is GLOBAL across libraries — a
        # second library registering an existing name is refused with
        # "Function <name> already exists" — so a row whose library LOADS
        # reserves 'f' and poisons every later row ON THAT ENGINE ONLY. That
        # made the CONTROL_valid row diverge for a reason having nothing to do
        # with what it measures: on fr the nil_index library still loads (o500d
        # row 4, unfixed) and takes 'f', while on redis it is rejected and 'f'
        # stays free. The rows must be independent for any of them to mean
        # anything.
        src = (SHEBANG % lib) + body.replace("'f'", f"'f{i}'")
        r_reply = classify(redis.cmd("FUNCTION", "LOAD", "REPLACE", src))
        f_reply = classify(fr.cmd("FUNCTION", "LOAD", "REPLACE", src))
        agree = (r_reply.split(":")[0] == f_reply.split(":")[0]) and (
            r_reply.startswith("ERR") == f_reply.startswith("ERR"))
        mark = "" if agree else "   <-- DIVERGES"
        if not agree:
            divergences += 1
            if name.startswith("CONTROL"):
                control_failures += 1
        print(f"{name:<22} {f_reply[:43]:<44} {r_reply[:43]}{mark}")

    print()
    if control_failures:
        print(f"HARNESS INVALID: {control_failures} CONTROL row(s) diverged — the probe "
              f"is not measuring what it claims. Fix the harness before trusting any row.")
        return 2
    print(f"{divergences} divergence(s) on the {len(CASES) - 2} behavioural rows; "
          f"both controls agree, so the probe discriminates.")
    return 1 if divergences else 0


if __name__ == "__main__":
    sys.exit(main())
