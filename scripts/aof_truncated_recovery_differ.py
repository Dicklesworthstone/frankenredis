#!/usr/bin/env python3
"""Exercise real AOF crash recovery against exact Redis 7.2.4.

Every case starts from fresh files, checks explicit expected replies, and retains
its input, logs, and report. Successful recovery is followed by another INCR and
a second restart, so accepting a damaged tail without repairing it cannot pass.
The candidate is required unless --oracle-only is explicit; that mode calibrates
the fixtures and does not claim anything about FrankenRedis.

Two short-read-inside-MULTI cases deliberately require stronger restart safety
from FrankenRedis. Redis 7.2.4 aof.c resets valid_up_to to valid_before_multi only
at clean EOF, not at readerr -> uxeof; a short read can leave MULTI on disk and
make the next strict restart fail. The report records those observed oracle
failures separately. The candidate must repair to the complete transaction
boundary and preserve its subsequently acknowledged INCR in every case.

Usage:
  aof_truncated_recovery_differ.py --redis /path/to/redis-server \
      --fr /path/to/frankenredis --output /path/to/new-results
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import time

from _respread import cmd, encode_command


def expect(actual, wanted, label):
    if actual != wanted:
        raise AssertionError(f"{label}: expected {wanted!r}, got {actual!r}")


class Server:
    def __init__(self, binary, directory, truncated, label):
        self.directory = directory
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            self.port = reservation.getsockname()[1]
        self.log_path = directory / f"{label}.log"
        self.log = self.log_path.open("xb")
        self.process = subprocess.Popen(
            [str(binary), "--bind", "127.0.0.1", "--port", str(self.port),
             "--dir", str(directory), "--appendonly", "yes",
             "--appenddirname", "appendonlydir", "--appendfilename", "appendonly.aof",
             "--appendfsync", "always", "--aof-load-truncated", truncated,
             "--save", ""],
            cwd=directory, stdout=self.log, stderr=subprocess.STDOUT,
        )
        self.connection = None

    def start(self):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                return False
            connection = None
            try:
                connection = socket.create_connection(("127.0.0.1", self.port), timeout=0.3)
                reply = cmd(connection, "PING")
                if reply.startswith(b"-LOADING "):
                    connection.close()
                    time.sleep(0.02)
                    continue
                expect(reply, b"+PONG\r\n", "startup PING")
                info = cmd(connection, "INFO", "server")
                if f"process_id:{self.process.pid}".encode() not in info.split(b"\r\n"):
                    raise AssertionError("connection does not belong to the spawned server")
                self.connection = connection
                return True
            except OSError:
                if connection is not None:
                    connection.close()
                time.sleep(0.02)
            except BaseException:
                if connection is not None:
                    connection.close()
                raise
        raise TimeoutError(f"startup timed out; see {self.log_path}")

    def stop(self, graceful=False):
        try:
            if self.process.poll() is None:
                if graceful and self.connection is not None:
                    # Redis closes the socket after a successful SHUTDOWN.
                    try:
                        reply = cmd(self.connection, "SHUTDOWN", "NOSAVE")
                    except OSError:
                        reply = b""
                    expect(reply, b"", "successful SHUTDOWN must close without a reply")
                    self.process.wait(timeout=5)
                    expect(self.process.returncode, 0, "successful shutdown exit")
                else:
                    self.process.kill()  # Only the disposable child created here.
                    self.process.wait(timeout=5)
        finally:
            if self.connection is not None:
                self.connection.close()
            self.log.close()


def seed_files(directory, layout, prefix, tail, damage_nonfinal=False):
    directory.mkdir(parents=True)
    files = {}
    if layout == "legacy":
        files["appendonly.aof"] = prefix + tail
    else:
        (directory / "appendonlydir").mkdir()
        files["appendonlydir/appendonly.aof.manifest"] = (
            b"file appendonly.aof.1.base.aof seq 1 type b\n"
            b"file appendonly.aof.1.incr.aof seq 1 type i\n"
        )
        if damage_nonfinal:
            files["appendonlydir/appendonly.aof.1.base.aof"] = prefix + tail
            files["appendonlydir/appendonly.aof.1.incr.aof"] = encode_command(("INCR", "counter"))
        else:
            files["appendonlydir/appendonly.aof.1.base.aof"] = prefix
            files["appendonlydir/appendonly.aof.1.incr.aof"] = tail
    for name, data in files.items():
        (directory / name).write_bytes(data)
    # Preserve exact original bytes independently of any server rewrite.
    (directory / "input.json").write_text(
        json.dumps({name: data.hex() for name, data in files.items()}, indent=2) + "\n"
    )
    return files


def check_dataset(connection, expected_counter):
    expect(cmd(connection, "GET", "counter"), f"${len(str(expected_counter))}\r\n{expected_counter}\r\n".encode(), "counter")
    expect(cmd(connection, "GET", "binary"), b"$4\r\n\x00\xff\r\n\r\n", "binary value")
    expect(cmd(connection, "EXISTS", "uncommitted", "torn"), b":0\r\n", "unfinished writes absent")
    expect(cmd(connection, "SELECT", 2), b"+OK\r\n", "select restored database")
    expect(cmd(connection, "GET", "other-db"), b"$5\r\nvalue\r\n", "database isolation")
    expect(cmd(connection, "SELECT", 0), b"+OK\r\n", "restore selected database")


def exercise(binary, directory, case, oracle):
    name, layout, prefix, tail, truncated, should_start, nonfinal = case
    originals = seed_files(directory, layout, prefix, tail, nonfinal)
    first = Server(binary, directory, truncated, "first-start")
    try:
        started = first.start()
        expect(started, should_start, f"{name} startup")
        if not started:
            expect(first.process.returncode, 1, f"{name} failed startup exit")
            log = first.log_path.read_text(errors="replace")
            if not any(marker in log for marker in (
                "Bad file format", "Unknown command", "Unexpected end of file",
                "not the last file", "failed to load AOF",
            )):
                raise AssertionError(f"{name}: exit did not establish an AOF load failure; see {first.log_path}")
            # Redis migrates a legacy AOF to appendonlydir before validating it.
            # FrankenRedis is free to leave it in place; either copy must be exact.
            for filename, data in originals.items():
                path = directory / filename
                if not path.exists() and layout == "legacy":
                    path = directory / "appendonlydir" / "appendonly.aof"
                # Redis 7.2.4 truncates a nonfinal file before rejecting the whole
                # generation. Do not require that destructive side effect from fr.
                if not nonfinal:
                    expect(path.read_bytes(), data, f"{name} rejected source preserved")
            return {"started": False, "exit_code": first.process.returncode}
        check_dataset(first.connection, 2)
        expect(cmd(first.connection, "INCR", "counter"), b":3\r\n", "post-recovery INCR")
        first.stop(graceful=True)
    finally:
        first.stop()

    second = Server(binary, directory, "no", "second-start")
    try:
        started = second.start()
        oracle_restart_defect = oracle and "multi-short-bulk-yes" in name
        expect(started, not oracle_restart_defect, "strict restart of repaired AOF")
        if oracle_restart_defect:
            expect(second.process.returncode, 1, "known oracle restart failure")
            if "Unexpected end of file" not in second.log_path.read_text(errors="replace"):
                raise AssertionError("oracle defect must be the retained unfinished transaction")
            return {"started": True, "strict_restart": False, "known_oracle_defect": "short read retains unfinished MULTI"}
        check_dataset(second.connection, 3)
        expect(cmd(second.connection, "INCR", "counter"), b":4\r\n", "second non-idempotent INCR")
        second.stop(graceful=True)
    finally:
        second.stop()
    return {"started": True, "strict_restart": True, "counter_after_restart": 3, "next_increment": 4}


def cases():
    prefix = b"".join(encode_command(args) for args in [
        ("SET", "counter", "0"), ("INCR", "counter"),
        ("MULTI",), ("INCR", "counter"),
        ("SET", "binary", b"\x00\xff\r\n"), ("EXEC",),
        ("SELECT", "2"), ("SET", "other-db", "value"), ("SELECT", "0"),
    ])
    tails = [
        ("short-bulk", encode_command(("SET", "torn", "value"))[:-3], True),
        ("short-array-header", b"*", True),
        ("open-multi", encode_command(("MULTI",)) + encode_command(("SET", "uncommitted", "value")), True),
        ("multi-short-bulk", encode_command(("MULTI",)) + encode_command(("SET", "uncommitted", "value")) + encode_command(("INCR", "counter"))[:-2], True),
        ("malformed-complete", b"!not-an-aof-command\r\n", False),
        ("malformed-short-type", b"+bad", False),
        ("malformed-short-argument", b"*2\r\n+SET\r\n", False),
        ("unknown-in-multi", encode_command(("MULTI",)) + encode_command(("NO_SUCH_REDIS_COMMAND",)), False),
    ]
    for layout in ("legacy", "multipart"):
        yield (f"{layout}-clean", layout, prefix, b"", "no", True, False)
        for tail_name, tail, repairable in tails:
            for truncated in ("yes", "no"):
                yield (f"{layout}-{tail_name}-{truncated}", layout, prefix, tail, truncated, repairable and truncated == "yes", False)
    yield ("multipart-nonfinal-short-bulk", "multipart", prefix, tails[0][1], "yes", False, True)
    yield ("multipart-nonfinal-open-multi", "multipart", prefix, tails[2][1], "yes", False, True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--redis", required=True, type=Path)
    parser.add_argument("--fr", type=Path)
    parser.add_argument("--oracle-only", action="store_true")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.oracle_only == (args.fr is not None):
        parser.error("provide exactly one of --fr or --oracle-only")
    binaries = {"redis": args.redis.resolve(strict=True)}
    if args.fr is not None:
        binaries["frankenredis"] = args.fr.resolve(strict=True)
    version = subprocess.check_output([str(binaries["redis"]), "--version"], text=True)
    if "v=7.2.4 " not in version or "sha=d2c8a4b9:0 " not in version:
        parser.error(f"oracle must be clean exact Redis 7.2.4 d2c8a4b9; got {version.strip()}")
    for binary in binaries.values():
        if not os.access(binary, os.X_OK):
            parser.error(f"binary is not executable: {binary}")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    report = {
        "evidence": "oracle-calibration" if args.oracle_only else "candidate-vs-oracle",
        "candidate_executed": not args.oracle_only,
        "oracle_version": version.strip(),
        "binaries": {label: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()} for label, path in binaries.items()},
        "cases": [],
    }
    try:
        for case in cases():
            results = {}
            for label, binary in binaries.items():
                results[label] = exercise(binary, args.output / f"{case[0]}-{label}", case, label == "redis")
            oracle_defect = "multi-short-bulk-yes" in case[0]
            if not args.oracle_only and not oracle_defect:
                expect(results["frankenredis"], results["redis"], f"{case[0]} differential")
            report["cases"].append({"name": case[0], "passed": True, "known_oracle_restart_defect": oracle_defect, "results": results})
            label = "OBSERVED ORACLE DEFECT" if oracle_defect and args.oracle_only else "PASS"
            print(f"{label} {case[0]}", flush=True)
        report["passed"] = True
    except BaseException as error:
        report["passed"] = False
        report["error"] = str(error)
        raise
    finally:
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"{report['evidence']}: {len(report['cases'])} cases passed; candidate_executed={report['candidate_executed']}")


if __name__ == "__main__":
    main()
