#!/usr/bin/env python3
"""Differentially triage OPEN capability beads against the live incumbent.

WHY THIS EXISTS. A bead's TITLE is a claim with a timestamp, and `br ready` ranks on titles.
On 2026-08-19 a triage of the six top ready capability beads found FIVE already fixed --
including `yx1wa`, filed as a SECURITY defect on "zero occurrences of 'denied due to
insufficient ACL' in the workspace" when there were four by the time anyone looked. Taking a
stale bead at face value costs a turn each; probing costs seconds and no build.

WHAT IT DOES: for each bead, runs that bead's OWN shape against fr and vendored Redis 7.2.4 in
one invocation and compares replies. AGREE means the described divergence is not reproducible.
DIVERGE means the bead is live.

WHAT AGREEMENT DOES **NOT** LICENSE, and this is the part that bit me. Agreement is evidence
about the BINARY YOU PROBED, not about HEAD, and `target/release/frankenredis` is a SHARED
artifact any agent may rebuild at any moment -- often from a tree carrying uncommitted work.
I closed `lua-tail-calls-ps0le` on a green probe against an ELF linked 12:45:04 while
`lua_eval.rs` was written at 12:59:13 and was dirty with the in-flight fix; the binary predated
the very work the bead tracked, and the bead had to be reopened. So:

    green probe  +  a COMMITTED fix you can name        ->  safe to close
    green probe  alone                                  ->  report only, do NOT close
    any dirty crates/ source                            ->  every row here is PROVISIONAL

This script therefore refuses to imply a close: it prints the provenance block first, marks all
rows PROVISIONAL when the tree is dirty, and never touches bead state.

Exit 0 when every bead not listed in KNOWN_LIVE agrees, 1 when one of them diverges (a bead
believed fixed has regressed, or a close was premature), 2 on a harness failure.
"""

from __future__ import annotations

import argparse
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
FR_DEFAULT = REPO / "target" / "release" / "frankenredis"
REDIS_DEFAULT = REPO / "legacy_redis_code" / "redis" / "src" / "redis-server"

# Beads measured LIVE and expected to diverge. A bead here that AGREES is good news worth
# checking, not a pass to ignore -- it is reported as STALE-EXPECTATION.
KNOWN_LIVE = {"lua-call-depth-ug22x"}


def start_server(cmd, port, name):
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    deadline = time.time() + 20.0
    while time.time() < deadline:
        # Bounded wait plus a liveness check: a port loop that never reads the writer waits
        # forever on a producer that already exited nonzero.
        if proc.poll() is not None:
            err = proc.stderr.read().decode("utf-8", "replace")[-800:] if proc.stderr else ""
            print(f"HARNESS FAILURE: {name} exited rc={proc.returncode} before listening\n{err}")
            sys.exit(2)
        try:
            s = socket.create_connection(("127.0.0.1", port), 0.3)
            s.close()
            return proc
        except OSError:
            time.sleep(0.15)
    proc.kill()
    print(f"HARNESS FAILURE: {name} never listened on {port} within 20s")
    sys.exit(2)


def encode(argv):
    out = b"*%d\r\n" % len(argv)
    for a in argv:
        b = a.encode() if isinstance(a, str) else a
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


class Conn:
    def __init__(self, port, timeout=20.0):
        self.s = socket.create_connection(("127.0.0.1", port), timeout)
        self.s.settimeout(timeout)
        self.buf = b""

    def _line(self):
        while b"\r\n" not in self.buf:
            d = self.s.recv(65536)
            if not d:
                raise EOFError("server closed")
            self.buf += d
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _read(self):
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag in (b"+", b":"):
            return rest.decode("latin1")
        if tag == b"-":
            return "ERR> " + rest.decode("latin1")
        if tag == b"$":
            n = int(rest)
            if n == -1:
                return None
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(65536)
            v, self.buf = self.buf[:n], self.buf[n + 2:]
            return v.decode("latin1")
        if tag in (b"*", b"~", b">"):
            n = int(rest)
            if n == -1:
                return None
            return [self._read() for _ in range(n)]
        if tag == b"%":
            n = int(rest)
            return {self._read(): self._read() for _ in range(n)}
        return line.decode("latin1")

    def cmd(self, *argv):
        try:
            self.s.sendall(encode(list(argv)))
            return self._read()
        except (socket.timeout, EOFError, OSError) as e:
            return f"<NO REPLY {type(e).__name__}>"

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def raw_exchange(port, payload, settle=0.6):
    """Send RAW bytes on a fresh connection and return (reply bytes, closed).

    Needed for shapes that are not well-formed commands -- an oversized length header
    never becomes an argv, so it cannot be sent through `Conn.cmd`.
    """
    try:
        s = socket.create_connection(("127.0.0.1", port), 2.0)
    except OSError as exc:
        return (f"<CONNECT FAILED {exc}>", True)
    out, closed = b"", False
    try:
        s.settimeout(settle)
        s.sendall(payload)
        while True:
            chunk = s.recv(65536)
            if not chunk:
                closed = True
                break
            out += chunk
            if len(out) > 65536:
                break
    except socket.timeout:
        pass
    except OSError:
        closed = True
    try:
        s.close()
    except OSError:
        pass
    return out.decode("latin1"), closed


# --------------------------------------------------------------------------------------
# One probe per bead. Each returns a dict of labelled replies; fr and redis must match.
# --------------------------------------------------------------------------------------

def probe_zy8kq(c):
    """LUA_MAXCAPTURES: fr matched patterns 7.2.4 refuses with 'too many captures'."""
    def caps(n):
        return c.cmd("EVAL", f"local s=string.rep('x',{n}) local p=string.rep('(x)',{n}) "
                             f"return tostring(string.match(s,p))", "0")
    return {"captures_31": caps(31), "captures_32": caps(32), "captures_40": caps(40)}


def probe_1tlyh(c):
    """thread stack_size: a cjson depth INSIDE upstream's limit exhausting a worker thread."""
    def enc(n):
        return c.cmd("EVAL", f"local t={{}} for i=1,{n} do t={{t}} end return cjson.encode(t)", "0")
    out = {"encode_900": enc(900), "encode_1000": enc(1000), "encode_1001": enc(1001)}
    out["still_alive"] = c.cmd("PING")
    return out


def probe_gvex0(c):
    """gsub parity: a bare '^' anchor, and EAGER validation of patterns upstream accepts."""
    return {
        "bare_caret_anchor": c.cmd("EVAL", "return (string.gsub('aaa','^a','b'))", "0"),
        "caret_mid_pattern": c.cmd("EVAL", "return (string.gsub('a^b','a^b','X'))", "0"),
        "unread_capture": c.cmd("EVAL", "return (string.gsub('abc','(a)','X'))", "0"),
        "backref_no_capture": c.cmd("EVAL", "return tostring(pcall(string.gsub,'abc','%1','X'))", "0"),
    }


def probe_mzkxl(c):
    """Protocol error strings fr emitted that 7.2.4 never sends ('exceeds limit').

    An oversized length header never becomes an argv, so this MUST go over raw bytes.
    The first version of this probe returned a canned note and a PING -- a row that could
    not fail, which inflates the agree count with a case that tests nothing. A vacuous
    AGREE is the same hazard as a false AGREE: nobody investigates agreement.
    """
    port = c.s.getpeername()[1]
    out = {}
    for label, payload in (("bulk_overflow", b"$536870913\r\n"),
                           ("multibulk_overflow", b"*2147483648\r\n"),
                           ("bulk_non_numeric", b"$x\r\n"),
                           ("multibulk_non_numeric", b"*x\r\n")):
        reply, closed = raw_exchange(port, payload)
        out[label] = (reply, closed)
    return out


def probe_xreadopts(c):
    """XREAD/XREADGROUP answering five upstream diagnostics with a generic syntax error."""
    c.cmd("DEL", "tri:s")
    c.cmd("XADD", "tri:s", "*", "f", "v")
    return {
        "noack_on_xread": c.cmd("XREAD", "NOACK", "STREAMS", "tri:s", "0"),
        "missing_group": c.cmd("XREADGROUP", "COUNT", "1", "STREAMS", "tri:s", ">"),
        "unbalanced_streams": c.cmd("XREAD", "STREAMS", "tri:s", "a", "b"),
        "count_non_numeric": c.cmd("XREAD", "COUNT", "x", "STREAMS", "tri:s", "0"),
        "block_non_numeric": c.cmd("XREAD", "BLOCK", "x", "STREAMS", "tri:s", "0"),
    }


def probe_monitorexec(c):
    """MONITOR inside MULTI/EXEC: upstream refuses it as a DENY BLOCKING client."""
    return {"multi": c.cmd("MULTI"),
            "queued": c.cmd("MONITOR"),
            "exec": c.cmd("EXEC")}


def probe_qeef0(c):
    """HELLO from a script: upstream never lets the command run there."""
    return {
        "hello_noargs": c.cmd("EVAL", "return tostring(pcall(redis.call,'HELLO'))", "0"),
        "hello_3": c.cmd("EVAL", "return tostring(pcall(redis.call,'HELLO','3'))", "0"),
        "hello_bogus": c.cmd("EVAL", "return tostring(pcall(redis.call,'HELLO','9'))", "0"),
    }


def probe_ug22x(c):
    """Lua self-recursion depth. Measured 767 (fr) vs 19998 (redis) by bisection."""
    src = ("local function f(n) if n<=0 then return 0 end return 1+f(n-1) end "
           "local ok,e=pcall(f,%d) return tostring(ok)")
    return {"depth_700": c.cmd("EVAL", src % 700, "0"),
            "depth_1000": c.cmd("EVAL", src % 1000, "0")}


def probe_control(c):
    """Control: a shape already proven at parity, so an all-agree table is not vacuous."""
    return {"ping": c.cmd("PING"), "echo": c.cmd("ECHO", "triage")}


CASES = [
    ("zy8kq", "LUA_MAXCAPTURES unimplemented", probe_zy8kq),
    ("thread-stack-size-1tlyh", "no thread stack_size anywhere", probe_1tlyh),
    ("gvex0", "gsub bare-caret anchor + eager validation", probe_gvex0),
    ("mzkxl", "invented protocol-error strings", probe_mzkxl),
    ("xreadopts-hhz9g", "five XREAD diagnostics collapsed to syntax error", probe_xreadopts),
    ("monitorexec-pfcz4", "MONITOR inside MULTI/EXEC", probe_monitorexec),
    ("qeef0", "HELLO from a script", probe_qeef0),
    ("lua-call-depth-ug22x", "self-recursion depth", probe_ug22x),
    ("CONTROL", "shapes already at parity", probe_control),
]


def provenance(fr_path):
    """Print what binary this is and refuse to let a dirty tree pass as HEAD."""
    sha = subprocess.run(["sha256sum", str(fr_path)], capture_output=True, text=True)
    sha = sha.stdout.split()[0] if sha.returncode == 0 else "<unreadable>"
    head = subprocess.run(["git", "-C", str(REPO), "log", "-1", "--format=%h %cI"],
                          capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", str(REPO), "status", "--porcelain", "crates/"],
                           capture_output=True, text=True).stdout.strip().splitlines()
    mtime = time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(fr_path.stat().st_mtime))
    print("PROVENANCE")
    print(f"  binary   {fr_path}")
    print(f"  sha256   {sha}")
    print(f"  linked   {mtime}")
    print(f"  HEAD     {head}")
    if dirty:
        print(f"  DIRTY    {len(dirty)} source(s) under crates/ are uncommitted:")
        for line in dirty:
            print(f"             {line}")
        print("  => target/release is a SHARED artifact and this tree is not HEAD.")
        print("     EVERY row below is PROVISIONAL. A green row is evidence about THIS binary")
        print("     only; do NOT close a bead on it without naming the commit that fixed it.")
    else:
        print("  clean    no uncommitted crates/ sources")
    try:
        load = Path("/proc/loadavg").read_text().split()[:3]
        print(f"  loadavg  {' '.join(load)}")
    except OSError:
        pass
    print()
    return bool(dirty)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fr", type=Path, default=FR_DEFAULT)
    ap.add_argument("--redis", type=Path, default=REDIS_DEFAULT)
    ap.add_argument("--fr-port", type=int, default=28911)
    ap.add_argument("--redis-port", type=int, default=28912)
    ap.add_argument("--only", default=None, help="comma-separated bead substrings to run")
    args = ap.parse_args()

    for label, path in (("fr", args.fr), ("redis", args.redis)):
        if not path.exists():
            print(f"SKIP: {label} binary not built at {path}")
            return 0

    provisional = provenance(args.fr)

    cases = CASES
    if args.only:
        wanted = [w.strip() for w in args.only.split(",")]
        cases = [c for c in cases if any(w in c[0] for w in wanted)]

    fr = start_server([str(args.fr), "--port", str(args.fr_port)], args.fr_port, "fr")
    redis = start_server(
        [str(args.redis), "--port", str(args.redis_port), "--save", ""],
        args.redis_port, "redis")

    rows = []
    try:
        for bead, claim, fn in cases:
            a = fn(Conn(args.fr_port))
            b = fn(Conn(args.redis_port))
            rows.append((bead, claim, a, b, a == b))
    finally:
        for proc in (fr, redis):
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    unexpected = []
    for bead, claim, a, b, same in rows:
        live = bead in KNOWN_LIVE
        if same and live:
            tag = "STALE-EXPECT"
        elif same:
            tag = "AGREE"
        elif live:
            tag = "LIVE (known)"
        else:
            tag = "DIVERGE"
            unexpected.append(bead)
        suffix = "  [PROVISIONAL]" if (same and provisional) else ""
        print(f"{tag:14s} {bead}  -- {claim}{suffix}")
        for k in a:
            if a[k] != b[k]:
                print(f"     * {k}")
                print(f"         fr     {a[k]!r}")
                print(f"         redis  {b[k]!r}")
    agreed = sum(1 for r in rows if r[4])
    print(f"\n{agreed}/{len(rows)} bead shapes agree with Redis 7.2.4")
    if provisional:
        print("PROVISIONAL: the tree has uncommitted crates/ sources. Green means "
              "'this binary passes', not 'this bead is fixed at HEAD'.")
    if unexpected:
        print(f"UNEXPECTED DIVERGENCE in {len(unexpected)}: {', '.join(unexpected)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
