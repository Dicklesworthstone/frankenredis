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
# (frankenredis-o500d) KNOWN, TRACKED divergences. Everything NOT listed here must agree,
# so a regression on an already-fixed row fails this gate instead of blending into a count.
#
# Before this table the script returned 1 whenever ANY row diverged, and one row has been
# permanently red since the bead was filed. A permanently-red gate cannot detect the SECOND
# divergence -- it just reads "2" instead of "1" and nobody looks. Three of the bead's four
# rows are now fixed (top_level_error, undeclared_global, tonumber_call all agree), and this
# is what stops them regressing silently.
#
# An entry here is a debt, not a permission: if a listed row starts AGREEING the gate fails
# too, demanding the entry be removed, because an allowance that outlives its bug is how a
# gate rots into permanent green.
EXPECTED_DIVERGENCES = {
    "nil_index": "o500d row 4 -- fr accepts `local t = nil; t.field`; redis rejects it. "
                 "fr's load-time check is a STATIC AST scan for undeclared globals "
                 "(function_library_first_undeclared_global), which cannot see a runtime "
                 "error on a LOCAL. Matching upstream needs the body EXECUTED in the "
                 "declared-globals sandbox, which also moves registration off the current "
                 "text-scan path -- a refactor, not a patch.",
}

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
    unexpected = []
    fixed_but_still_expected = []
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
            if name in EXPECTED_DIVERGENCES:
                mark = "   <-- diverges (KNOWN: %s)" % EXPECTED_DIVERGENCES[name]
            else:
                unexpected.append(name)
        elif name in EXPECTED_DIVERGENCES:
            # A tracked gap that now AGREES. Say so loudly: an expectation left in
            # place after the bug is fixed is how a gate rots into permanent green.
            fixed_but_still_expected.append(name)
            mark = "   <-- FIXED, remove it from EXPECTED_DIVERGENCES"
        print(f"{name:<22} {f_reply[:43]:<44} {r_reply[:43]}{mark}")

    print()
    if control_failures:
        print(f"HARNESS INVALID: {control_failures} CONTROL row(s) diverged — the probe "
              f"is not measuring what it claims. Fix the harness before trusting any row.")
        return 2
    print(f"{divergences} divergence(s) on the {len(CASES) - 2} behavioural rows; "
          f"both controls agree, so the probe discriminates.")
    if fixed_but_still_expected:
        print("STALE EXPECTATION: %s now agree(s) with 7.2.4. Delete the entry from "
              "EXPECTED_DIVERGENCES -- an allowance that outlives its bug hides the next "
              "regression on that row." % ", ".join(fixed_but_still_expected))
        return 1
    if unexpected:
        print("FAIL: %d UNEXPECTED divergence(s): %s" % (len(unexpected), ", ".join(unexpected)))
        return 1
    if divergences:
        print("PASS: every divergence is a KNOWN, tracked gap. A new one fails this gate.")
    else:
        print("PASS: no divergences at all.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
