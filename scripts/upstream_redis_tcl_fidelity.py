#!/usr/bin/env python3
"""Run a curated, non-vacuous slice of Redis 7.2.4's unmodified Tcl tests against FrankenRedis.

This is deliberately different from FrankenRedis's bespoke differential corpus. It executes
the upstream Redis test harness itself in external-server mode, pinned to the exact Redis
7.2.4 release commit, and fails if any selected test disappears, is skipped, runs twice, or
fails. A green result therefore means the named upstream assertions actually executed.

It is not a claim that the complete Redis test suite passes. The selected set avoids tests
that assert Redis's internal encodings/process topology rather than its client-visible API,
and is intended to grow monotonically.
"""

from __future__ import annotations

import argparse
import json
import re
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REDIS_724_COMMIT = "d2c8a4b91e8c0e6aefd1f5bc0bf582cddbe046b7"

SUITES: dict[str, tuple[str, ...]] = {
    "unit/type/string": (
        "SET and GET an item",
        "SET and GET an empty item",
        "SETNX target key missing",
        "SETNX target key exists",
        "GETDEL command",
        "MGET",
        "MGET against non existing key",
        "MGET against non-string key",
        "GETSET (set new value)",
        "GETSET (replace old value)",
        "MSET base case",
        "MSET/MSETNX wrong number of args",
        "STRLEN against non-existing key",
        "STRLEN against plain string",
    ),
    "unit/type/incr": (
        "INCR against non existing key",
        "INCR against key originally set with SET",
        "INCR over 32bit value",
        "INCRBY over 32bit value with over 32bit increment",
        "INCR fails against key with spaces (left)",
        "INCR fails against key with spaces (right)",
        "INCR fails against key with spaces (both)",
        "DECRBY negation overflow",
        "INCR fails against a key holding a list",
        "DECRBY over 32bit value with over 32bit increment, negative res",
        "DECRBY against key is not exist",
    ),
    "unit/keyspace": (
        "DEL against a single item",
        "Vararg DEL",
        "EXISTS",
        "Zero length value in key. SET/GET/EXISTS",
        "Non existing command",
        "RENAME basic usage",
        "RENAME against already existing key",
        "RENAMENX basic usage",
        "RENAMENX against already existing key",
        "RENAME against non existing source key",
        "RENAME where source and dest key are the same (existing)",
        "RENAMENX where source and dest key are the same (existing)",
        "RENAME where source and dest key are the same (non existing)",
    ),
    "unit/multi": (
        "MULTI / EXEC basics",
        "DISCARD",
        "Nested MULTI are not allowed",
        "WATCH inside MULTI is not allowed",
        "EXEC fails if there are errors while queueing commands #1",
        "If EXEC aborts, the client MULTI state is cleared",
        "EXEC works on WATCHed key not modified",
        "EXEC fail on WATCHed key modified (1 key of 1 watched)",
        "EXEC fail on WATCHed key modified (1 key of 5 watched)",
        "After successful EXEC key is no longer watched",
        "After failed EXEC key is no longer watched",
        "It is possible to UNWATCH",
        "UNWATCH when there is nothing watched works as expected",
        "FLUSHALL is able to touch the watched keys",
        "FLUSHALL does not touch non affected keys",
        "FLUSHDB is able to touch the watched keys",
        "FLUSHDB does not touch non affected keys",
        "DISCARD should clear the WATCH dirty flag on the client",
        "DISCARD should UNWATCH all the keys",
    ),
    "unit/protocol": (
        "Handle an empty query",
        "Negative multibulk length",
        "Out of range multibulk length",
        "Wrong multibulk payload header",
        "Negative multibulk payload length",
        "Out of range multibulk payload length",
        "Non-number multibulk payload length",
        "Multi bulk request not followed by bulk arguments",
        "Generic wrong number of args",
        "Unbalanced number of quotes",
        "raw protocol response",
        "raw protocol response - deferred",
        "raw protocol response - multiline",
        "test large number of args",
        "test argument rewriting - issue 9598",
    ),
}

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
OK_RE = re.compile(r"^\[ok\]: (.+) \(\d+ ms\)$")


def run_checked(cmd: list[str], cwd: Path | None = None) -> str:
    result = subprocess.run(cmd, cwd=cwd, text=True, capture_output=True, timeout=30)
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise RuntimeError(f"command failed ({result.returncode}): {' '.join(cmd)}\n{detail}")
    return result.stdout.strip()


def verify_upstream(upstream: Path) -> None:
    helper = upstream / "tests" / "test_helper.tcl"
    runtest = upstream / "runtest"
    if not helper.is_file() or not runtest.is_file():
        raise RuntimeError(f"{upstream} is not a Redis source tree with runtest and tests/test_helper.tcl")
    head = run_checked(["git", "-C", str(upstream), "rev-parse", "HEAD"])
    if head != REDIS_724_COMMIT:
        raise RuntimeError(
            f"wrong Redis source revision: expected {REDIS_724_COMMIT}, got {head or '<empty>'}"
        )


def resp_ping(port: int) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.5) as sock:
            sock.sendall(b"*1\r\n$4\r\nPING\r\n")
            sock.settimeout(0.5)
            return sock.recv(64).startswith(b"+PONG")
    except OSError:
        return False


def wait_ready(proc: subprocess.Popen[bytes], port: int) -> None:
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"FrankenRedis exited before listening (rc={proc.returncode})")
        if resp_ping(port):
            return
        time.sleep(0.1)
    raise RuntimeError(f"FrankenRedis did not become ready on port {port}")


def parse_passed(output: str) -> list[str]:
    passed: list[str] = []
    for raw in output.splitlines():
        line = ANSI_RE.sub("", raw).strip()
        match = OK_RE.match(line)
        if match:
            passed.append(match.group(1))
    return passed


def run_unit(upstream: Path, port: int, unit: str, expected: tuple[str, ...]) -> dict[str, object]:
    cmd = [
        str(upstream / "runtest"),
        "--host", "127.0.0.1",
        "--port", str(port),
        "--clients", "1",
        "--ignore-encoding",
        "--single", unit,
    ]
    for name in expected:
        cmd.extend(["--only", name])
    result = subprocess.run(
        cmd,
        cwd=upstream,
        text=True,
        capture_output=True,
        timeout=180,
    )
    combined = result.stdout + ("\n" + result.stderr if result.stderr else "")
    passed = parse_passed(combined)
    expected_set = set(expected)
    passed_set = set(passed)
    duplicates = sorted(name for name in passed_set if passed.count(name) != 1)
    missing = sorted(expected_set - passed_set)
    unexpected = sorted(passed_set - expected_set)
    ok = result.returncode == 0 and not duplicates and not missing and not unexpected
    return {
        "unit": unit,
        "ok": ok,
        "exit_code": result.returncode,
        "expected_count": len(expected),
        "passed_count": len(passed),
        "missing": missing,
        "duplicates": duplicates,
        "unexpected": unexpected,
        "output": combined,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--upstream", type=Path, default=Path("legacy_redis_code/redis"))
    parser.add_argument("--fr", type=Path, required=True)
    parser.add_argument("--port", type=int, default=29171)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()

    try:
        verify_upstream(args.upstream)
        if not args.fr.is_file():
            raise RuntimeError(f"FrankenRedis binary not found: {args.fr}")

        selected = sum(len(names) for names in SUITES.values())
        reports: list[dict[str, object]] = []
        with tempfile.TemporaryDirectory(prefix="fr-upstream-tcl-") as tmp:
            proc = subprocess.Popen(
                [str(args.fr.resolve()), "--port", str(args.port), "--save", ""],
                cwd=tmp,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                wait_ready(proc, args.port)
                for unit, names in SUITES.items():
                    print(f"upstream Tcl: {unit} ({len(names)} selected tests)")
                    report = run_unit(args.upstream.resolve(), args.port, unit, names)
                    reports.append(report)
                    status = "PASS" if report["ok"] else "FAIL"
                    print(
                        f"  {status}: {report['passed_count']}/{report['expected_count']} "
                        f"selected assertions executed"
                    )
                    if not report["ok"]:
                        if report["missing"]:
                            print(f"  missing: {report['missing']}", file=sys.stderr)
                        if report["duplicates"]:
                            print(f"  duplicates: {report['duplicates']}", file=sys.stderr)
                        if report["unexpected"]:
                            print(f"  unexpected: {report['unexpected']}", file=sys.stderr)
                        tail = str(report["output"]).splitlines()[-40:]
                        print("\n".join(tail), file=sys.stderr)
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)

        passed = sum(int(report["passed_count"]) for report in reports)
        all_ok = len(reports) == len(SUITES) and all(bool(report["ok"]) for report in reports)
        payload = {
            "schema_version": "fr_upstream_redis_tcl_fidelity/v1",
            "redis_version": "7.2.4",
            "redis_commit": REDIS_724_COMMIT,
            "selected_count": selected,
            "passed_count": passed,
            "all_selected_executed_exactly_once": all_ok and passed == selected,
            "units": [
                {k: v for k, v in report.items() if k != "output"}
                for report in reports
            ],
        }
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        print(
            f"UPSTREAM REDIS 7.2.4 TCL FIDELITY: {passed}/{selected} "
            f"selected upstream assertions passed"
        )
        return 0 if payload["all_selected_executed_exactly_once"] else 1
    except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"HARNESS FAILURE: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
