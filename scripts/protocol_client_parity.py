#!/usr/bin/env python3
"""Byte-exact CLIENT-PATH protocol parity: fr vs vendored Redis 7.2.4, over real sockets.

WHY THIS EXISTS (frankenredis-vqiki). `crates/fr-conformance/fixtures/protocol_negative.json`
looks like it covers this surface and does not. Its driver calls
`runtime.execute_bytes(...)`, whose own doc comment says it is an INTERNAL LINK -- "never raw
bytes off a client connection, which the front-end parses before anything reaches here". So the
suite compares fr's internal entry point against expectations transcribed from fr itself, and on
this input set NONE of its 22 expectations describe what a client receives. Measured:

    input                      fixture asserts                     both engines actually send
    $-2\\r\\n                    invalid bulk length                 unknown command '$-2'
    *-2\\r\\n                    invalid multibulk length            (no reply, connection open)
    *2\\r\\n$4\\r\\nPING\\r\\n~1\\r\\n   unsupported RESP3 type prefix '~'   expected '$', got '~'

A fixture written from the implementation cannot detect the implementation being wrong. That is
why the divergence it appears to cover survived: the test that looks like it covers this input is
the reason nothing flagged it.

WHY A SOCKET AND NOT AN IN-PROCESS MODEL. Modelling the front-end in-process was tried and
abandoned on evidence: `*-2\\r\\n` draws NO reply from either engine, while `parse_frame_with_config`
reports `invalid multibulk length`. The front-end's multibulk handling is therefore not
reconstructable from the parser alone, and any in-process model would encode a fiction. The
front-end is only faithfully exercised through the thing clients use.

WHAT IT ASSERTS: for every vector, fr and the incumbent must agree on the reply BYTES and on
whether the connection was closed. The incumbent is the oracle; fr's answers are never the
expectation. A vector both engines answer identically is parity even when neither matches what
some fixture predicted -- which is the whole point.

Exit 0 when every vector agrees, 1 on any divergence, 2 on a harness failure (a binary missing,
a server that never listened). Skips cleanly with 0 when a binary is absent so it can sit in CI
before the vendored tree is built.
"""

from __future__ import annotations

import argparse
import json
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
FR_DEFAULT = REPO / "target" / "release" / "frankenredis"
REDIS_DEFAULT = REPO / "legacy_redis_code" / "redis" / "src" / "redis-server"

# Every vector is raw bytes written to a fresh connection. Grouped by what they probe.
# The RESP-prefix block is upstream networking.c:2543-2550: any first byte that is not '*'
# starts an INLINE command, so these are "unknown command" replies with the connection kept
# open -- not protocol errors.
VECTORS: list[tuple[str, bytes]] = [
    # --- RESP type prefixes, all inline-reachable ---
    ("resp3_set_prefix", b"~1\r\n"),
    ("resp3_map_prefix", b"%1\r\n"),
    ("resp3_bool_prefix", b"#t\r\n"),
    ("resp3_push_prefix", b">1\r\n"),
    ("resp3_blob_error_prefix", b"!\r\n"),
    ("resp3_attribute_prefix", b"|1\r\n"),
    ("resp3_double_prefix", b",1.5\r\n"),
    ("resp3_bignum_prefix", b"(123\r\n"),
    ("resp3_verbatim_prefix", b"=4\r\n"),
    ("resp3_null_prefix", b"_\r\n"),
    ("unknown_prefix_question", b"?\r\n"),
    ("simple_string_prefix", b"+OK\r\n"),
    ("error_prefix", b"-ERR nope\r\n"),
    ("integer_prefix", b":abc\r\n"),
    ("integer_prefix_numeric", b":42\r\n"),
    # --- bulk prefixes: '$' is not '*', so these are inline too ---
    ("bulk_negative", b"$-2\r\n"),
    ("bulk_noncanonical_null", b"$-01\r\n"),
    ("bulk_non_numeric", b"$x\r\n"),
    ("bulk_length_overflow", b"$536870913\r\n"),
    ("bulk_incomplete", b"$3\r\nab"),
    # --- the multibulk path, the only prefix that stays on the RESP parser ---
    ("multibulk_negative", b"*-2\r\n"),
    ("multibulk_noncanonical_null", b"*-01\r\n"),
    ("multibulk_non_numeric", b"*x\r\n"),
    ("multibulk_length_overflow", b"*2147483648\r\n"),
    ("multibulk_zero", b"*0\r\n"),
    ("multibulk_incomplete_tail", b"*2\r\n$4\r\nPING\r\n$3\r\nab"),
    ("multibulk_nested_resp3", b"*2\r\n$4\r\nPING\r\n~1\r\n"),
    ("multibulk_expects_bulk", b"*1\r\n+PING\r\n"),
    ("multibulk_deep_nesting", b"*1\r\n" * 128 + b":42\r\n"),
    ("multibulk_bulk_len_mismatch", b"*1\r\n$100\r\nPING\r\n"),
    # --- inline command handling proper ---
    ("inline_ping", b"PING\r\n"),
    ("inline_ping_lf_only", b"PING\n"),
    ("inline_unknown_command", b"NOSUCHCOMMAND\r\n"),
    ("inline_with_args", b"ECHO hello\r\n"),
    ("inline_quoted_arg", b'ECHO "hello world"\r\n'),
    ("inline_single_quoted_arg", b"ECHO 'hello world'\r\n"),
    ("inline_unbalanced_quote", b'ECHO "unclosed\r\n'),
    ("inline_empty_line", b"\r\n"),
    ("inline_only_spaces", b"   \r\n"),
    ("inline_leading_space", b"  PING\r\n"),
    ("inline_two_commands", b"PING\r\nPING\r\n"),
    ("inline_then_multibulk", b"PING\r\n*1\r\n$4\r\nPING\r\n"),
    ("inline_incomplete_no_newline", b"+OK\r"),
    ("inline_escaped_hex", b'ECHO "\\x41\\x42"\r\n'),
    ("inline_tab_separated", b"ECHO\thello\r\n"),
]


def start_server(cmd: list[str], port: int, name: str) -> subprocess.Popen:
    """Start a server and wait, WITH A DEADLINE, until it actually accepts."""
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    deadline = time.time() + 20.0
    while time.time() < deadline:
        # A file-sentinel or port loop that never checks the writer waits forever on a
        # producer that already died. Check liveness every iteration and read its stderr.
        if proc.poll() is not None:
            err = proc.stderr.read().decode("utf-8", "replace")[-800:] if proc.stderr else ""
            print(f"HARNESS FAILURE: {name} exited rc={proc.returncode} before listening\n{err}")
            sys.exit(2)
        try:
            probe = socket.create_connection(("127.0.0.1", port), 0.3)
            probe.close()
            return proc
        except OSError:
            time.sleep(0.15)
    proc.kill()
    print(f"HARNESS FAILURE: {name} never listened on {port} within 20s")
    sys.exit(2)


def probe(port: int, raw: bytes, settle: float) -> tuple[bytes, bool]:
    """Send raw bytes on a FRESH connection; return (reply bytes, connection closed)."""
    try:
        sock = socket.create_connection(("127.0.0.1", port), 2.0)
    except OSError as exc:
        return (f"<CONNECT FAILED: {exc}>".encode(), True)
    out = b""
    closed = False
    try:
        sock.settimeout(settle)
        sock.sendall(raw)
        while True:
            chunk = sock.recv(65536)
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
    if not closed:
        # Distinguish "kept the connection open" from "closed after replying".
        try:
            sock.settimeout(settle / 2)
            closed = sock.recv(16) == b""
        except socket.timeout:
            closed = False
        except OSError:
            closed = True
    try:
        sock.close()
    except OSError:
        pass
    return out, closed


def render(raw: bytes, limit: int = 72) -> str:
    text = repr(raw)
    return text if len(text) <= limit else text[: limit - 4] + "...'"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fr", type=Path, default=FR_DEFAULT)
    ap.add_argument("--redis", type=Path, default=REDIS_DEFAULT)
    ap.add_argument("--fr-port", type=int, default=28871)
    ap.add_argument("--redis-port", type=int, default=28872)
    ap.add_argument("--settle", type=float, default=0.6,
                    help="seconds to wait for a reply before calling it silence")
    ap.add_argument("--json", type=Path, default=None, help="write the full table here")
    ap.add_argument("--extra-redis-arg", action="append", default=[],
                    help="extra argv passed to the incumbent. Also the MUTATION TEST for this "
                         "harness: '--extra-redis-arg --requirepass --extra-redis-arg secret' "
                         "makes the oracle answer NOAUTH everywhere, and a run that still "
                         "reports full agreement is a BROKEN harness, not a passing gate.")
    args = ap.parse_args()

    for label, path in (("fr", args.fr), ("redis", args.redis)):
        if not path.exists():
            print(f"SKIP: {label} binary not built at {path}")
            return 0

    fr = start_server([str(args.fr), "--port", str(args.fr_port)], args.fr_port, "fr")
    redis = start_server(
        [str(args.redis), "--port", str(args.redis_port), "--save", ""] + args.extra_redis_arg,
        args.redis_port, "redis",
    )

    rows = []
    try:
        for name, raw in VECTORS:
            fr_out, fr_closed = probe(args.fr_port, raw, args.settle)
            rd_out, rd_closed = probe(args.redis_port, raw, args.settle)
            rows.append({
                "name": name,
                "raw": raw.decode("latin1"),
                "fr": fr_out.decode("latin1"),
                "fr_closed": fr_closed,
                "redis": rd_out.decode("latin1"),
                "redis_closed": rd_closed,
                "agree": fr_out == rd_out and fr_closed == rd_closed,
            })
    finally:
        for proc in (fr, redis):
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    if args.json:
        args.json.write_text(json.dumps(rows, indent=1), encoding="utf-8")

    diverged = [r for r in rows if not r["agree"]]
    for row in diverged:
        print(f"DIVERGE  {row['name']}")
        print(f"    sent   {render(row['raw'].encode('latin1'))}")
        print(f"    fr     {render(row['fr'].encode('latin1'))}  closed={row['fr_closed']}")
        print(f"    redis  {render(row['redis'].encode('latin1'))}  closed={row['redis_closed']}")

    agreed = len(rows) - len(diverged)
    print(f"\nCLIENT-PATH PARITY: {agreed}/{len(rows)} vectors agree byte-for-byte with Redis 7.2.4")
    if diverged:
        print(f"{len(diverged)} DIVERGENT -- fr's client protocol contract differs from the incumbent")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
