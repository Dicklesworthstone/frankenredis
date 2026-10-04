#!/usr/bin/env python3
"""Exercise hash-pinned prebuilt FrankenRedis binaries over real RESP sockets.

The caller verifies release signatures and supplies both binary hashes. This
runner builds nothing, touches no existing dataset, and retains every log/RDB.
Its receipt qualifies the supplied artifacts, never an unrelated source tree.
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

    def check(self, name, condition):
        if not condition:
            raise RuntimeError(f"failed check: {name}")
        self.checks.append(name)
        with (self.output / "events.jsonl").open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({"check": name, "passed": True}) + "\n")


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


class OwnedServer:
    def __init__(self, binary, version, data, label, receipt):
        self.binary, self.version, self.data = binary, version, data
        self.label, self.receipt = label, receipt
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
                [str(self.binary), "--bind", "127.0.0.1", "--port", str(port),
                 "--dir", str(self.data), "--dbfilename", "dump.rdb",
                 "--appendonly", "no", "--save", ""],
                cwd=self.data, stdout=self.log, stderr=subprocess.STDOUT,
            )
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if self.process.poll() is not None:
                    raise RuntimeError(f"{self.label} exited during startup")
                if f"FrankenRedis v{self.version} ready" in self.log_path.read_text(errors="replace"):
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
