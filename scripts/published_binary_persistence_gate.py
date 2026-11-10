#!/usr/bin/env python3
"""Exercise hash-pinned prebuilt FrankenRedis binaries over real RESP sockets.

The caller verifies release signatures and supplies both binary hashes. This
runner builds nothing, touches no existing dataset, and retains every log/RDB.
Its receipt qualifies the supplied artifacts, never an unrelated source tree.
Checksum-valid function-library snapshots also exercise startup rejection through
standalone RDB, legacy AOF preambles, and multipart AOF bases.
"""

import argparse
import hashlib
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import time
import traceback
from pathlib import Path


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


class Receipt:
    def __init__(self, output):
        self.output = output
        self.checks = []

    def check(self, name, condition, **observed):
        if not condition:
            raise RuntimeError(f"failed check: {name}")
        self.checks.append(name)
        with (self.output / "events.jsonl").open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({"check": name, "passed": True, **observed}) + "\n")


class RespConnection:
    def __init__(self, port):
        self.socket = socket.create_connection(("127.0.0.1", port), timeout=2)
        self.stream = self.socket.makefile("rb")

    def close(self):
        self.stream.close()
        self.socket.close()

    def read(self, depth=0):
        if depth > 32:
            raise ValueError("RESP nesting exceeds test protocol limit")
        line = self.stream.readline(8193)
        if not line:
            raise ConnectionError("server closed the connection")
        if len(line) > 8192 or not line.endswith(b"\r\n"):
            raise ValueError("invalid RESP line")
        kind, value = line[:1], line[1:-2]
        if kind == b"+":
            return value
        if kind == b"-":
            raise RuntimeError(f"RESP error: {value!r}")
        if kind == b":":
            return int(value)
        if kind in (b"$", b"*"):
            count = int(value)
            if count == -1:
                return None
            if count < 0 or count > (2 * 1024 * 1024 if kind == b"$" else 1024):
                raise ValueError("RESP length exceeds test protocol limit")
            if kind == b"*":
                return [self.read(depth + 1) for _ in range(count)]
            payload = self.stream.read(count + 2)
            if len(payload) != count + 2 or payload[-2:] != b"\r\n":
                raise ValueError("truncated RESP bulk")
            return payload[:-2]
        raise ValueError(f"unsupported RESP kind: {kind!r}")

    def command(self, *args):
        parts = [b"*%d\r\n" % len(args)]
        for arg in args:
            value = arg if isinstance(arg, bytes) else str(arg).encode()
            parts.extend((b"$%d\r\n" % len(value), value, b"\r\n"))
        self.socket.sendall(b"".join(parts))
        return self.read()


def startup_command(binary, data, port, persistence_options):
    return [str(binary), "--bind", "127.0.0.1", "--port", str(port),
            "--dir", str(data), "--dbfilename", "dump.rdb", "--save", "",
            *persistence_options]


class OwnedServer:
    def __init__(self, binary, version, data, label, receipt, *,
                 persistence_options=("--appendonly", "no"), ready_marker=None):
        self.binary, self.version, self.data = binary, version, data
        self.label, self.receipt = label, receipt
        self.persistence_options = persistence_options
        self.ready_marker = ready_marker or f"FrankenRedis v{version} ready"
        self.process = self.connection = self.log = None
        self.owns_connection = False
        self.log_path = receipt.output / f"{label}.log"

    def __enter__(self):
        self.data.mkdir(parents=True, exist_ok=True)
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        self.log = self.log_path.open("xb")
        try:
            self.process = subprocess.Popen(
                startup_command(self.binary, self.data, port, self.persistence_options),
                cwd=self.data, stdout=self.log, stderr=subprocess.STDOUT,
            )
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    raise RuntimeError(f"{self.label} exited during startup")
                if self.ready_marker in self.log_path.read_text(errors="replace"):
                    try:
                        self.connection = RespConnection(port)
                        break
                    except OSError:
                        time.sleep(0.05)
                else:
                    time.sleep(0.05)
            if self.connection is None:
                raise RuntimeError(f"{self.label} did not become ready")
            self.receipt.check(f"{self.label}:ping", self.cmd("PING") == b"PONG")
            info = self.cmd("INFO", "server")
            self.receipt.check(
                f"{self.label}:owned-process",
                isinstance(info, bytes) and f"process_id:{self.process.pid}".encode()
                in info.split(b"\r\n"),
            )
            self.owns_connection = True
            return self
        except BaseException:
            self.stop(require_graceful=False)
            raise

    def cmd(self, *args):
        return self.connection.command(*args)

    def stop(self, require_graceful=True):
        forced = False
        try:
            if self.process is not None and self.process.poll() is None:
                if self.connection is not None and self.owns_connection:
                    try:
                        self.cmd("SHUTDOWN", "NOSAVE")
                    except (ConnectionError, OSError, RuntimeError, ValueError):
                        pass
                try:
                    self.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    forced = True
                    self.process.terminate()  # Only the child created by this instance.
                    try:
                        self.process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        self.process.kill()
                        self.process.wait(timeout=5)
        finally:
            if self.connection is not None:
                self.connection.close()
            if self.log is not None:
                self.log.close()
        if require_graceful:
            self.receipt.check(
                f"{self.label}:graceful-shutdown",
                not forced and self.process is not None and self.process.returncode == 0,
            )

    def __exit__(self, exc_type, exc_value, exc_traceback):
        self.stop(require_graceful=exc_type is None)


def pinned_binary(path, expected, receipt, label):
    binary = path.resolve(strict=True)
    receipt.check(f"{label}:executable", binary.is_file() and os.access(binary, os.X_OK))
    receipt.check(f"{label}:sha256", sha256(binary) == expected)
    return binary


def crc64_redis(payload):
    checksum = 0
    for byte in payload:
        checksum ^= byte
        for _ in range(8):
            checksum = (checksum >> 1) ^ (0x95AC9329AC4BC9B5 if checksum & 1 else 0)
    return checksum


def rdb_string(value):
    length = len(value)
    if length < 64:
        return bytes([length]) + value
    if length < 16384:
        return bytes([0x40 | (length >> 8), length & 0xFF]) + value
    return b"\x80" + length.to_bytes(4, "big") + value


def function_library_rdb(libraries):
    """Build real FUNCTION2 records, keeping the container and CRC valid."""
    body = bytearray(b"REDIS0011")
    for library in libraries:
        body.extend(b"\xf5" + rdb_string(library))
    body.extend(b"\xfe\x00\x00" + rdb_string(b"function-marker") + rdb_string(b"loaded"))
    body.extend(b"\xfe\x02\x00" + rdb_string(b"database-marker") + rdb_string(b"second"))
    body.extend(b"\xff")
    return bytes(body) + crc64_redis(body).to_bytes(8, "little")


def function_startup_data(output, label, snapshot, mode, *, redis_oracle=False):
    data = output / f"{label}-data"
    data.mkdir(exist_ok=False)
    tail = b"*3\r\n$3\r\nSET\r\n$11\r\ntail-marker\r\n$7\r\npresent\r\n"
    if mode == "rdb":
        (data / "dump.rdb").write_bytes(snapshot)
        return data, ("--appendonly", "no")
    if mode == "legacy-aof":
        legacy = data / "appendonly.aof"
        legacy.write_bytes(snapshot + tail)
        # Redis migrates the legacy file into appendonlydir before loading it;
        # FrankenRedis's explicit --aof path selects its single-file loader.
        # Both consume the same preamble and tail bytes. The original fixture
        # remains outside this owned dataset in the retained receipt.
        options = ("--appendonly", "yes") if redis_oracle else ("--aof", str(legacy))
        return data, options
    if mode != "multipart-aof":
        raise ValueError(f"unknown persistence mode: {mode}")
    append_dir = data / "appendonlydir"
    append_dir.mkdir()
    (append_dir / "appendonly.aof.1.base.rdb").write_bytes(snapshot)
    (append_dir / "appendonly.aof.1.incr.aof").write_bytes(tail)
    (append_dir / "appendonly.aof.manifest").write_text(
        "file appendonly.aof.1.base.rdb seq 1 type b\n"
        "file appendonly.aof.1.incr.aof seq 1 type i\n", encoding="utf-8",
    )
    return data, ("--appendonly", "yes")


def persistence_file_bytes(data):
    files = {}
    for path in sorted(data.rglob("*")):
        if path.is_symlink():
            raise RuntimeError(f"unexpected persistence symlink: {path}")
        if path.is_file():
            files[path.relative_to(data).as_posix()] = path.read_bytes()
    return files


def check_persistence_files(data, expected, label, receipt):
    actual = persistence_file_bytes(data)
    receipt.check(
        f"{label}:persistence-file-paths",
        actual.keys() == expected.keys(),
        expected_paths=sorted(expected), actual_paths=sorted(actual),
    )
    for path, payload in expected.items():
        receipt.check(
            f"{label}:persistence-file-bytes:{path}", actual[path] == payload,
            expected_sha256=hashlib.sha256(payload).hexdigest(),
            actual_sha256=hashlib.sha256(actual[path]).hexdigest(),
        )


def rejected_function_startup(binary, data, label, options, receipt, *,
                              ready_marker, mode, redis_oracle=False):
    # Snapshot the files the server will actually consume, not just the retained
    # reference fixture outside its data directory. Failed candidate startup may
    # neither replace these bytes nor publish a new persistence generation.
    expected_files = persistence_file_bytes(data)
    if redis_oracle and mode == "legacy-aof":
        # Upstream migrates the legacy file before validating its preamble.
        # Account for that exact oracle-only move and manifest, without allowing
        # candidate startup to move, overwrite, or create persistence files.
        expected_files = {
            "appendonlydir/appendonly.aof": expected_files["appendonly.aof"],
            "appendonlydir/appendonly.aof.manifest": b"file appendonly.aof seq 1 type b\n",
        }
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    log_path = receipt.output / f"{label}.log"
    with log_path.open("xb") as log:
        process = subprocess.Popen(
            startup_command(binary, data, port, options),
            cwd=data, stdout=log, stderr=subprocess.STDOUT,
        )
        try:
            returncode = process.wait(timeout=10)
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
    # Redis's standalone-RDB failure path invokes redis-check-rdb, whose
    # structural success can exit 0 despite the preceding function-load error.
    # Record that oracle behavior explicitly. Published FrankenRedis binaries
    # still MUST return a nonzero status; no CLI option relaxes that contract.
    receipt.check(
        f"{label}:startup-exit",
        returncode in (0, 1) if redis_oracle else returncode > 0,
        exit_code=returncode, target="redis-oracle" if redis_oracle else "candidate",
    )
    diagnostic = log_path.read_text(errors="replace").lower()
    receipt.check(
        f"{label}:load-failure-diagnostic",
        any(message in diagnostic for message in
            ("failed to load", "failed loading library", "failed creating function")),
    )
    receipt.check(f"{label}:never-ready", ready_marker.lower() not in diagnostic)
    check_persistence_files(data, expected_files, label, receipt)


def exercise_function_library_startup(binary, version, receipt, *, redis_oracle=False):
    """The same fixture matrix can run against an independently pinned oracle.

    A positive control for every entry mode prevents a bad launch configuration
    from making all the rejection cases pass. A socket accepting connections is
    insufficient: each positive control verifies the spawned process ID, restored
    keys, an evaluated registration, and incremental AOF replay.
    """
    receipt.check("function-startup:crc64-reference", crc64_redis(b"123456789") == 0xE9C6D914C4B8D9CA)
    ready_marker = "Ready to accept connections" if redis_oracle else f"FrankenRedis v{version} ready"
    computed = (
        b"#!lua name=checkpoint\nlocal suffix = '_fn'\n"
        b"redis.register_function('computed' .. suffix, function() return 'restored' end)"
    )
    cases = [
        ("computed-registration", [computed], True),
        ("runtime-error", [computed,
            b"#!lua name=runtime_bad\n"
            b"redis.register_function('staged', function() return 1 end)\n"
            b"local fail = false\nfail()"], False),
        ("syntax-error", [computed,
            b"#!lua name=syntax_bad\nredis.register_function("], False),
        ("no-registrations", [computed,
            b"#!lua name=empty_library\nlocal unused = 1"], False),
        ("duplicate-library", [computed, computed], False),
        ("function-collision", [computed,
            b"#!lua name=collision\n"
            b"redis.register_function('computed_fn', function() return 99 end)"], False),
    ]
    for case, libraries, valid in cases:
        snapshot = function_library_rdb(libraries)
        original = receipt.output / f"function-{case}.rdb"
        with original.open("xb") as stream:
            stream.write(snapshot)
        original_hash = sha256(original)
        for mode in ("rdb", "legacy-aof", "multipart-aof"):
            label = f"function-{case}-{mode}"
            data, options = function_startup_data(
                receipt.output, label, snapshot, mode, redis_oracle=redis_oracle,
            )
            if not valid:
                rejected_function_startup(
                    binary, data, label, options, receipt,
                    ready_marker=ready_marker, mode=mode, redis_oracle=redis_oracle,
                )
                continue
            with OwnedServer(binary, version, data, label, receipt,
                             persistence_options=options,
                             ready_marker=ready_marker) as server:
                receipt.check(f"{label}:restored-key", server.cmd("GET", "function-marker") == b"loaded")
                receipt.check(f"{label}:computed-function", server.cmd("FCALL", "computed_fn", 0) == b"restored")
                receipt.check(f"{label}:database-select", server.cmd("SELECT", 2) == b"OK")
                receipt.check(f"{label}:second-database", server.cmd("GET", "database-marker") == b"second")
                receipt.check(f"{label}:function-global", server.cmd("FCALL", "computed_fn", 0) == b"restored")
                receipt.check(f"{label}:default-database", server.cmd("SELECT", 0) == b"OK")
                receipt.check(f"{label}:aof-tail", server.cmd("GET", "tail-marker") ==
                              (None if mode == "rdb" else b"present"))
        receipt.check(f"function-{case}:original-rdb-unchanged", sha256(original) == original_hash)


def exercise(args, receipt, binary, previous):
    fresh = receipt.output / "fresh-data"
    large = b"release-e2e-" * 65536
    with OwnedServer(binary, args.version, fresh, "fresh", receipt) as server:
        receipt.check("fresh:default-fec-disabled", server.cmd("CONFIG", "GET", "rdb-fec") == [b"rdb-fec", b"no"])
        receipt.check("fresh:set-large", server.cmd("SET", "large", large) == b"OK")
        receipt.check("fresh:get-large", server.cmd("GET", "large") == large)
        receipt.check("fresh:list-write", server.cmd("LPUSH", "list", "a", "b") == 2)
        receipt.check("fresh:hash-write", server.cmd("HSET", "hash", "field", "value") == 1)
        receipt.check("fresh:set-write", server.cmd("SADD", "set", "a", "b") == 2)
        receipt.check("fresh:multi", server.cmd("MULTI") == b"OK")
        receipt.check("fresh:queued-set", server.cmd("SET", "tx", "committed") == b"QUEUED")
        receipt.check("fresh:queued-get", server.cmd("GET", "tx") == b"QUEUED")
        receipt.check("fresh:exec", server.cmd("EXEC") == [b"OK", b"committed"])
        receipt.check("fresh:ttl-write", server.cmd("SET", "ttl", "short", "PX", 300) == b"OK")
        deadline = time.monotonic() + 3
        while server.cmd("GET", "ttl") is not None and time.monotonic() < deadline:
            time.sleep(0.02)
        receipt.check("fresh:ttl-expired", server.cmd("GET", "ttl") is None)
        receipt.check("fresh:save", server.cmd("SAVE") == b"OK")
    receipt.check("fresh:primary-rdb", (fresh / "dump.rdb").is_file())
    receipt.check("fresh:no-default-sidecars", not list(fresh.rglob("*.symbols")) and not list(fresh.rglob("*.envelope.json")))
    with OwnedServer(binary, args.version, fresh, "restart", receipt) as server:
        receipt.check("restart:string", server.cmd("GET", "large") == large)
        receipt.check("restart:list", server.cmd("LRANGE", "list", 0, -1) == [b"b", b"a"])
        receipt.check("restart:hash", server.cmd("HGET", "hash", "field") == b"value")
        receipt.check("restart:set", server.cmd("SCARD", "set") == 2)
        receipt.check("restart:transaction", server.cmd("GET", "tx") == b"committed")
        receipt.check("restart:ttl", server.cmd("GET", "ttl") is None)
    old_data = receipt.output / "previous-data"
    with OwnedServer(previous, args.previous_version, old_data, "previous", receipt) as server:
        receipt.check("previous:seed", server.cmd("SET", "previous-key", "retained") == b"OK")
        receipt.check("previous:save", server.cmd("SAVE") == b"OK")
    old_hash = sha256(old_data / "dump.rdb")
    upgraded = receipt.output / "upgrade-data"
    shutil.copytree(old_data, upgraded)  # Fresh destination; original bytes stay intact.
    with OwnedServer(binary, args.version, upgraded, "upgrade", receipt) as server:
        receipt.check("upgrade:old-value", server.cmd("GET", "previous-key") == b"retained")
        receipt.check("upgrade:default-fec-disabled", server.cmd("CONFIG", "GET", "rdb-fec") == [b"rdb-fec", b"no"])
        receipt.check("upgrade:write", server.cmd("SET", "new-key", "current") == b"OK")
        receipt.check("upgrade:save", server.cmd("SAVE") == b"OK")
    with OwnedServer(binary, args.version, upgraded, "upgrade-restart", receipt) as server:
        receipt.check("upgrade-restart:old", server.cmd("GET", "previous-key") == b"retained")
        receipt.check("upgrade-restart:new", server.cmd("GET", "new-key") == b"current")
    receipt.check("upgrade:no-default-sidecars", not list(upgraded.rglob("*.symbols")) and not list(upgraded.rglob("*.envelope.json")))
    receipt.check("previous:original-rdb-unchanged", sha256(old_data / "dump.rdb") == old_hash)
    exercise_function_library_startup(binary, args.version, receipt)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--version", required=True, help="Version without leading v")
    parser.add_argument("--previous-binary", type=Path, required=True)
    parser.add_argument("--previous-sha256", required=True)
    parser.add_argument("--previous-version", required=True)
    parser.add_argument("--output", type=Path, required=True, help="New retained evidence directory")
    args = parser.parse_args()
    for value in (args.sha256, args.previous_sha256):
        if len(value) != 64 or any(char not in "0123456789abcdef" for char in value):
            parser.error("hashes must be lowercase SHA-256 hex")
    output = args.output.absolute()
    output.mkdir(parents=True, exist_ok=False)
    receipt = Receipt(output)
    report = {"passed": False, "platform": platform.platform(), "checks": receipt.checks,
              "scope": "supplied prebuilt binaries; no source-build or signature qualification",
              "current": {"version": args.version, "sha256": args.sha256},
              "previous": {"version": args.previous_version, "sha256": args.previous_sha256}}
    try:
        binary = pinned_binary(args.binary, args.sha256, receipt, "current")
        previous = pinned_binary(args.previous_binary, args.previous_sha256, receipt, "previous")
        exercise(args, receipt, binary, previous)
        receipt.check("current:binary-unchanged", sha256(binary) == args.sha256)
        receipt.check("previous:binary-unchanged", sha256(previous) == args.previous_sha256)
        report["passed"] = True
    except Exception:  # noqa: BLE001 - Preserve the failure receipt and return nonzero.
        report["failure"] = traceback.format_exc()
    finally:
        report["check_count"] = len(receipt.checks)
        with (output / "report.json").open("x", encoding="utf-8") as stream:
            json.dump(report, stream, indent=2)
            stream.write("\n")
    print(json.dumps({"passed": report["passed"], "check_count": report["check_count"], "report": str(output / "report.json")}))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
