#!/usr/bin/env python3
"""Run Redis 7.2.4's complete default Tcl suite against FrankenRedis in external mode.

Redis's own test harness has first-class support for an external server via --host/--port.
This runner deliberately does not copy or translate the Tcl tests: it executes the pinned,
unmodified Redis 7.2.4 test tree itself. Tests tagged by upstream as external:skip are
ignored by upstream's own tags_acceptable logic; every default test unit must still be
scheduled and complete exactly once, so an empty/partial run cannot look green.

The process exit status is Redis's real suite verdict, strengthened with anti-vacuity checks:
wrong upstream revision, missing units, duplicate units, an early FrankenRedis exit, or zero
executed assertions are harness failures. A JSON report records pass/skip/ignore/error counts
and the exact units completed.
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
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
OK_RE = re.compile(r"^\[ok\]: (.+?) \(\d+ ms\)$")
SKIP_RE = re.compile(r"^\[skip\]: (.+)$")
IGNORE_RE = re.compile(r"^\[ignore\]: (.+)$")
ERR_RE = re.compile(r"^\[err\]: (.+)$")
DONE_RE = re.compile(r"^\[\d+/\d+ done\]: (.+?) \(\d+ seconds\)$")


def run_checked(cmd: list[str], cwd: Path | None = None, timeout: int = 60) -> str:
    result = subprocess.run(cmd, cwd=cwd, text=True, capture_output=True, timeout=timeout)
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
        raise RuntimeError(f"wrong Redis source revision: expected {REDIS_724_COMMIT}, got {head or '<empty>'}")


def list_units(upstream: Path) -> list[str]:
    raw = run_checked([str(upstream / "runtest"), "--list-tests"], cwd=upstream)
    units = [line.strip() for line in raw.splitlines() if line.strip()]
    if not units:
        raise RuntimeError("upstream runtest --list-tests returned no units")
    if len(units) != len(set(units)):
        dupes = sorted({unit for unit in units if units.count(unit) > 1})
        raise RuntimeError(f"upstream unit list contains duplicates: {dupes}")
    return units


def resp_ping(port: int) -> bool:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.5) as sock:
            sock.sendall(b"*1\r\n$4\r\nPING\r\n")
            sock.settimeout(0.5)
            return sock.recv(64).startswith(b"+PONG")
    except OSError:
        return False


def wait_ready(proc: subprocess.Popen[bytes], port: int) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"FrankenRedis exited before listening (rc={proc.returncode})")
        if resp_ping(port):
            return
        time.sleep(0.1)
    raise RuntimeError(f"FrankenRedis did not become ready on port {port}")


def clean_lines(output: str) -> list[str]:
    return [ANSI_RE.sub("", raw).strip() for raw in output.splitlines()]


def matches(lines: list[str], regex: re.Pattern[str]) -> list[str]:
    out: list[str] = []
    for line in lines:
        match = regex.match(line)
        if match:
            out.append(match.group(1))
    return out


def parse_report(output: str, expected_units: list[str], exit_code: int, server_exit_code: int | None) -> dict[str, object]:
    lines = clean_lines(output)
    passed = matches(lines, OK_RE)
    skipped = matches(lines, SKIP_RE)
    ignored = matches(lines, IGNORE_RE)
    errors = matches(lines, ERR_RE)
    completed = matches(lines, DONE_RE)
    completed_set = set(completed)
    expected_set = set(expected_units)
    duplicate_units = sorted(unit for unit in completed_set if completed.count(unit) != 1)
    missing_units = sorted(expected_set - completed_set)
    unexpected_units = sorted(completed_set - expected_set)
    all_units_exactly_once = not duplicate_units and not missing_units and not unexpected_units and len(completed) == len(expected_units)
    anti_vacuity_ok = all_units_exactly_once and len(passed) > 0
    suite_passed = exit_code == 0 and anti_vacuity_ok and server_exit_code is None
    return {
        "schema_version": "fr_upstream_redis_tcl_full/v1",
        "redis_version": "7.2.4",
        "redis_commit": REDIS_724_COMMIT,
        "suite_exit_code": exit_code,
        "frankenredis_early_exit_code": server_exit_code,
        "expected_unit_count": len(expected_units),
        "completed_unit_count": len(completed),
        "passed_assertion_count": len(passed),
        "skipped_assertion_count": len(skipped),
        "ignored_assertion_count": len(ignored),
        "error_count": len(errors),
        "all_units_completed_exactly_once": all_units_exactly_once,
        "anti_vacuity_ok": anti_vacuity_ok,
        "suite_passed": suite_passed,
        "missing_units": missing_units,
        "duplicate_units": duplicate_units,
        "unexpected_units": unexpected_units,
        "completed_units": completed,
        "errors": errors,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--upstream", type=Path, default=Path("legacy_redis_code/redis"))
    parser.add_argument("--fr", type=Path, required=True)
    parser.add_argument("--port", type=int, default=29181)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--log", type=Path)
    parser.add_argument("--timeout", type=int, default=7200)
    args = parser.parse_args()

    try:
        upstream = args.upstream.resolve()
        verify_upstream(upstream)
        expected_units = list_units(upstream)
        if not args.fr.is_file():
            raise RuntimeError(f"FrankenRedis binary not found: {args.fr}")

        print(f"Redis 7.2.4 default upstream units: {len(expected_units)}")
        print("Running the complete upstream suite in external-server mode; upstream external:skip tags remain authoritative.")
        cmd = [
            str(upstream / "runtest"),
            "--host", "127.0.0.1",
            "--port", str(args.port),
            "--clients", "1",
            "--ignore-encoding",
            "--ignore-digest",
            "--no-latency",
        ]
        with tempfile.TemporaryDirectory(prefix="fr-upstream-tcl-full-") as tmp:
            proc = subprocess.Popen(
                [str(args.fr.resolve()), "--port", str(args.port), "--save", "", "--appendonly", "no", "--enable-debug-command", "yes"],
                cwd=tmp,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                wait_ready(proc, args.port)
                result = subprocess.run(cmd, cwd=upstream, text=True, capture_output=True, timeout=args.timeout)
                combined = result.stdout + ("\n" + result.stderr if result.stderr else "")
                early_exit = proc.poll()
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait(timeout=5)

        report = parse_report(combined, expected_units, result.returncode, early_exit)
        if args.log:
            args.log.parent.mkdir(parents=True, exist_ok=True)
            args.log.write_text(combined, encoding="utf-8", errors="replace")
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

        print(
            "UPSTREAM REDIS 7.2.4 FULL TCL: "
            f"units={report['completed_unit_count']}/{report['expected_unit_count']} "
            f"pass={report['passed_assertion_count']} skip={report['skipped_assertion_count']} "
            f"ignore={report['ignored_assertion_count']} errors={report['error_count']} "
            f"exit={report['suite_exit_code']}"
        )
        if report["missing_units"]:
            print(f"missing units: {report['missing_units']}", file=sys.stderr)
        if report["duplicate_units"]:
            print(f"duplicate units: {report['duplicate_units']}", file=sys.stderr)
        if report["unexpected_units"]:
            print(f"unexpected units: {report['unexpected_units']}", file=sys.stderr)
        if early_exit is not None:
            print(f"FrankenRedis exited during the suite with rc={early_exit}", file=sys.stderr)
        return 0 if report["suite_passed"] else 1
    except subprocess.TimeoutExpired as exc:
        print(f"HARNESS FAILURE: upstream suite timed out after {exc.timeout}s", file=sys.stderr)
        return 2
    except (OSError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"HARNESS FAILURE: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
