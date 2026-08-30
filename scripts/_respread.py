#!/usr/bin/env python3
"""Shared RESP client for the differential gates. (frankenredis-r9ei8)

Not a gate — this module has no side effects and nothing to run. It exists so
the three corrections below have ONE home instead of being hand-copied into
every differ, where they drift back one file at a time.

See docs/GATE_VALIDITY.md for the full catalogue of ways a differential gate
passes without testing anything, and the checklist to run before adding one.

Each correction fixes a way a gate can be GREEN FOR A REASON UNRELATED TO THE
PROPERTY IT TESTS, all three of which were measured on real gates in this repo:

1. TRUNCATING READ. `sleep(0.02); sock.recv(N)` returns whatever has ARRIVED,
   not a complete RESP frame. Measured: a 4000-member ZRANGEBYLEX came back as
   exactly 65536 bytes of a ~96KB reply. Both sides of a differ use the same
   reader, so two replies truncated at the same offset COMPARE EQUAL while
   differing past it — a false PASS, which is worse than a false FAIL because
   nothing looks wrong. `cmd()` here reads until a whole frame is buffered.

2. FIXTURE RE-ENCODING. A fixture written as a str escape (`"\\xff"`) sent
   through a bare `.encode()` becomes utf-8 — 0xff turns into 0xc3 0xbf, which
   is VALID UTF-8. Gates written specifically to exercise non-UTF8 handling
   therefore stopped exercising it. `encode_arg()` uses latin-1 so a str escape
   maps to the single byte it names.

   NOTE the deliberate exception: fixtures that are real TEXT rather than byte
   escapes (accented words in a collation gate) SHOULD be utf-8, because that is
   what a real client sends. Pass bytes explicitly in that case.

3. UNVERIFIED SEED. If seeding silently fails, most commands answer 0 / -1 /
   empty for a missing key on BOTH engines, so the gate passes while testing
   nothing. `assert_seed()` makes a counting seed (RPUSH/SADD/...) a checked
   precondition, and `assert_ok()` does the same for a `+OK` seed (SET/MSET/...).

Usage:

    from _respread import conn, cmd, assert_seed

    s = conn(port)
    cmd(s, "FLUSHALL")
    assert_seed(cmd(s, "RPUSH", "k", "a", "b"), 2, "RPUSH k")
    reply = cmd(s, "LRANGE", "k", "0", "-1")   # complete frame, raw bytes

Scripts run as `python3 scripts/<gate>.py`, so the scripts directory is
sys.path[0] and a plain `import _respread` resolves.
"""
import socket
import sys
import time

__all__ = ["conn", "cmd", "cmd_n", "read_frame", "encode_arg", "encode_command",
           "frame_len", "assert_seed", "assert_ok"]


def encode_arg(x):
    """One RESP argument as bytes.

    bytes pass through untouched; str is encoded latin-1 so that a "\\xNN"
    escape means the single byte NN rather than its utf-8 expansion; anything
    else is str()'d first (ints are the common case).
    """
    if isinstance(x, bytes):
        return x
    if not isinstance(x, str):
        x = str(x)
    return x.encode("latin-1")


def encode_command(args):
    """A RESP array command from an iterable of arguments."""
    args = [encode_arg(a) for a in args]
    out = b"*%d\r\n" % len(args)
    for a in args:
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def frame_len(buf, i=0):
    """Byte length of the complete RESP frame at buf[i:], or None if partial.

    Recursive so nested aggregates (arrays of arrays, maps, RESP3 pushes) are
    measured correctly rather than by counting CRLFs.
    """
    nl = buf.find(b"\r\n", i)
    if nl < 0:
        return None
    kind, head = buf[i:i + 1], buf[i + 1:nl]
    # simple string, error, integer, double, boolean, big number
    if kind in (b"+", b"-", b":", b",", b"#", b"("):
        return nl + 2 - i
    if kind in (b"$", b"=", b"!"):  # bulk string, verbatim string, bulk error
        n = int(head)
        if n < 0:  # RESP2 null bulk
            return nl + 2 - i
        end = nl + 2 + n + 2
        return end - i if len(buf) >= end else None
    if kind in (b"*", b"~", b">", b"%"):  # array, set, push, map
        n = int(head)
        if n < 0:  # RESP2 null array
            return nl + 2 - i
        if kind == b"%":
            n *= 2  # a map declares PAIRS
        pos = nl + 2
        for _ in range(n):
            sub = frame_len(buf, pos)
            if sub is None:
                return None
            pos += sub
        return pos - i
    if kind == b"_":  # RESP3 null
        return nl + 2 - i
    raise ValueError(f"unrecognised RESP type byte {kind!r}")


def conn(port, host="127.0.0.1", timeout=10):
    return socket.create_connection((host, port), timeout=timeout)


def read_frame(sock, deadline=10):
    """Read until ONE complete RESP frame is buffered; return its raw bytes.

    The read half of `cmd`, exposed separately for the gates that build their own
    wire bytes (hand-built packets, deliberately malformed frames) or that keep
    send and receive as separate steps. Those had each hand-rolled this loop,
    which is exactly the drift `_respread` exists to prevent.
    """
    expires_at = time.monotonic() + deadline
    prior_timeout = sock.gettimeout() if hasattr(sock, "gettimeout") else None
    can_set_timeout = hasattr(sock, "settimeout")
    buf = b""
    try:
        while True:
            try:
                if buf and frame_len(buf) is not None:
                    return buf
            except ValueError:
                # Not RESP we recognise — hand it back rather than spinning; the
                # caller's comparison will surface it.
                return buf
            remaining = expires_at - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out after {deadline}s waiting for complete RESP reply")
            if can_set_timeout:
                sock.settimeout(remaining)
            chunk = sock.recv(1 << 20)
            if not chunk:
                raise OSError("server closed the connection mid-reply")
            buf += chunk
    finally:
        if can_set_timeout:
            sock.settimeout(prior_timeout)


def cmd(sock, *args, deadline=10):
    """Send one command; return the COMPLETE raw reply bytes.

    Raw bytes rather than a decoded value is deliberate: a decoded comparison
    hides encoding differences (integer vs bulk, RESP2 nil spelling, element
    order) that a client would actually observe.
    """
    sock.sendall(encode_command(args))
    return read_frame(sock, deadline=deadline)


def cmd_n(sock, n, *args, deadline=10):
    """Send one command; return the COMPLETE raw bytes of its FIRST `n` frames.

    A fourth way a gate reads less than it thinks (frankenredis-tesrb/gpry6): some
    commands answer with one frame PER ARGUMENT rather than one frame total.
    `SSUBSCRIBE a b c` emits three confirmations, `SUNSUBSCRIBE` one per channel.
    `cmd()` returns as soon as the FIRST frame is complete, so it would capture one
    of three and leave the rest in the buffer for whatever reads next — both engines
    lose the same frames and still compare equal.

    Use this wherever the reply count is known and > 1. For frames nobody solicited
    (a delivered pub/sub message), no count is knowable in advance and a timed drain
    is the only correct shape — see the drain() docstrings in the pub/sub gates.
    """
    sock.sendall(encode_command(args))
    expires_at = time.monotonic() + deadline
    prior_timeout = sock.gettimeout() if hasattr(sock, "gettimeout") else None
    can_set_timeout = hasattr(sock, "settimeout")
    buf = b""
    try:
        while True:
            pos = got = 0
            try:
                while got < n:
                    ln = frame_len(buf, pos)
                    if ln is None:
                        break
                    pos += ln
                    got += 1
            except ValueError:
                return buf  # not RESP we recognise — hand it back, don't spin
            if got >= n:
                return buf
            remaining = expires_at - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out after {deadline}s waiting for {n} RESP replies")
            if can_set_timeout:
                sock.settimeout(remaining)
            chunk = sock.recv(1 << 20)
            if not chunk:
                raise OSError("server closed the connection mid-reply")
            buf += chunk
    finally:
        if can_set_timeout:
            sock.settimeout(prior_timeout)


def assert_ok(reply, label, exit_code=1):
    """Fail loudly unless `reply` is `+OK`.

    The `assert_seed` companion for seeds that are SET/MSET/etc rather than a
    counting write. Same reason it exists: a silently failed seed leaves the
    cases operating on a missing key, where most commands answer identically on
    both engines and the gate passes having exercised nothing.
    """
    if reply != b"+OK\r\n":
        print(f"SEED FAILED [{label}]: got {reply!r}, expected b'+OK\\r\\n'")
        sys.exit(exit_code)
    return reply


def assert_seed(reply, expected_int, label, exit_code=1):
    """Fail loudly unless `reply` is the integer `expected_int`.

    A silently failed seed leaves the cases operating on a missing key, and most
    commands answer 0 / -1 / empty for a missing key on BOTH engines — so the
    gate passes while testing nothing at all.
    """
    want = b":%d\r\n" % expected_int
    if reply != want:
        print(f"SEED FAILED [{label}]: got {reply!r}, expected {want!r}")
        sys.exit(exit_code)
    return reply


class _ChunkedSocket:
    """Minimal socket double for the reader validity checks below."""

    def __init__(self, chunks):
        self.chunks = list(chunks)
        self.sent = []

    def recv(self, _size):
        return self.chunks.pop(0) if self.chunks else b""

    def sendall(self, data):
        self.sent.append(data)


def _legacy_single_recv(sock):
    """The pre-tesrb read shape, retained only to prove the counterfactual."""
    return sock.recv(1 << 20)


def _expect_seed_failure(check, *args):
    try:
        check(*args)
    except SystemExit as exc:
        return exc.code == 1
    return False


def _self_test() -> int:
    """Plant replies the pre-tesrb reader would falsely compare as equal.

    Each pair shares its first TCP delivery and differs only in a later frame
    tail. A legacy ``recv()`` once returns the shared prefix on both sides, so a
    differential gate passes. The real primitive must collect enough bytes for
    the reply boundary, at which point the deliberately wrong reply is visible.
    The second case covers commands such as SSUBSCRIBE that have a known count
    of reply frames rather than one aggregate reply.
    """
    failures = []

    prefix = b"$65540\r\n" + b"x" * 65536
    oracle = prefix + b"good\r\n"
    wrong = prefix + b"FAIL\r\n"
    legacy_oracle = _legacy_single_recv(_ChunkedSocket([prefix, b"good\r\n"]))
    legacy_wrong = _legacy_single_recv(_ChunkedSocket([prefix, b"FAIL\r\n"]))
    if legacy_oracle != legacy_wrong:
        failures.append("test setup: legacy single receive did not share a prefix")
    observed_oracle = read_frame(_ChunkedSocket([prefix, b"good\r\n"]))
    observed_wrong = read_frame(_ChunkedSocket([prefix, b"FAIL\r\n"]))
    if observed_oracle != oracle or observed_wrong != wrong:
        failures.append("single-frame reader did not return the complete planted replies")
    elif observed_oracle == observed_wrong:
        failures.append("single-frame reader did not catch the planted wrong reply")

    first = b">3\r\n+ssubscribe\r\n$2\r\ns1\r\n:1\r\n"
    second_oracle = b">3\r\n+smessage\r\n$2\r\ns1\r\n$4\r\ngood\r\n"
    second_wrong = b">3\r\n+smessage\r\n$2\r\ns1\r\n$4\r\nFAIL\r\n"
    legacy_first_oracle = _legacy_single_recv(_ChunkedSocket([first, second_oracle]))
    legacy_first_wrong = _legacy_single_recv(_ChunkedSocket([first, second_wrong]))
    if legacy_first_oracle != legacy_first_wrong:
        failures.append("test setup: first confirmation is not shared")
    multi_oracle_sock = _ChunkedSocket([first, second_oracle])
    multi_wrong_sock = _ChunkedSocket([first, second_wrong])
    observed_oracle = cmd_n(multi_oracle_sock, 2, "SSUBSCRIBE", "s1")
    observed_wrong = cmd_n(multi_wrong_sock, 2, "SSUBSCRIBE", "s1")
    if observed_oracle != first + second_oracle or observed_wrong != first + second_wrong:
        failures.append("known-count reader did not return both planted reply frames")
    elif observed_oracle == observed_wrong:
        failures.append("known-count reader did not catch the planted wrong reply")
    if not multi_oracle_sock.sent or not multi_wrong_sock.sent:
        failures.append("known-count reader did not send its command")

    try:
        read_frame(_ChunkedSocket([]), deadline=0)
    except TimeoutError:
        pass
    else:
        failures.append("reader did not enforce its bounded deadline")

    if not _expect_seed_failure(assert_seed, b":0\r\n", 1, "planted seed"):
        failures.append("assert_seed accepted a deliberately wrong reply")
    if not _expect_seed_failure(assert_ok, b":1\r\n", "planted OK seed"):
        failures.append("assert_ok accepted a deliberately wrong reply")

    if failures:
        print("SELF-TEST FAIL:")
        for failure in failures:
            print(f"  {failure}")
        return 1
    print("SELF-TEST PASS: complete-frame, known-count, and seed gates catch planted wrong replies")
    return 0


if __name__ == "__main__":
    raise SystemExit(_self_test() if "--self-test" in sys.argv else 0)
