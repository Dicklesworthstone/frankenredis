from __future__ import annotations

import argparse
import hashlib
import json
import re
import socket
import subprocess
import tempfile
import time
from pathlib import Path


def encode_command(parts: list[bytes]) -> bytes:
    out = bytearray()
    out.extend(f"*{len(parts)}\r\n".encode())
    for part in parts:
        out.extend(f"${len(part)}\r\n".encode())
        out.extend(part)
        out.extend(b"\r\n")
    return bytes(out)


def read_until_crlf(sock: socket.socket) -> bytes:
    data = bytearray()
    while not data.endswith(b"\r\n"):
        chunk = sock.recv(1)
        if not chunk:
            raise RuntimeError("server closed connection while reading line")
        data.extend(chunk)
    return bytes(data)


def read_exact(sock: socket.socket, size: int) -> bytes:
    data = bytearray()
    while len(data) < size:
        chunk = sock.recv(size - len(data))
        if not chunk:
            raise RuntimeError("server closed connection while reading bulk")
        data.extend(chunk)
    return bytes(data)


def read_frame(sock: socket.socket) -> bytes:
    first = sock.recv(1)
    if not first:
        raise RuntimeError("server closed connection")
    if first in (b"+", b"-", b":"):
        return first + read_until_crlf(sock)
    if first == b"$":
        header = read_until_crlf(sock)
        size = int(header[:-2])
        if size < 0:
            return first + header
        return first + header + read_exact(sock, size + 2)
    if first == b"*":
        header = read_until_crlf(sock)
        count = int(header[:-2])
        payload = bytearray(first + header)
        for _ in range(count):
            payload.extend(read_frame(sock))
        return bytes(payload)
    raise RuntimeError(f"unsupported RESP frame prefix {first!r}")


def bulk_payload(frame: bytes) -> bytes:
    if not frame.startswith(b"$"):
        raise RuntimeError(f"expected bulk frame, got {frame[:64]!r}")
    header_end = frame.index(b"\r\n")
    size = int(frame[1:header_end])
    if size < 0:
        return b""
    start = header_end + 2
    end = start + size
    return frame[start:end]


def request(sock: socket.socket, parts: list[bytes]) -> bytes:
    sock.sendall(encode_command(parts))
    return read_frame(sock)


def wait_ready(port: int) -> None:
    deadline = time.time() + 10.0
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2) as sock:
                if request(sock, [b"PING"]) == b"+PONG\r\n":
                    return
        except OSError:
            time.sleep(0.02)
    raise RuntimeError(f"server on port {port} did not become ready")


def request_shutdown(port: int) -> None:
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.5) as sock:
            sock.sendall(encode_command([b"SHUTDOWN", b"NOSAVE"]))
            try:
                read_frame(sock)
            except RuntimeError:
                pass
    except OSError:
        pass


def rss_kb(pid: int) -> int:
    with Path(f"/proc/{pid}/status").open("r", encoding="utf-8") as status:
        for line in status:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    raise RuntimeError(f"VmRSS missing for pid {pid}")


def list_value(index: int, payload_size: int) -> bytes:
    value = f"v:{index:08}:".encode()
    if len(value) < payload_size:
        value += bytes([ord("a") + index % 26]) * (payload_size - len(value))
    return value[:payload_size]


def load_list(port: int, key: bytes, list_len: int, payload_size: int, batch: int) -> float:
    start = time.perf_counter()
    with socket.create_connection(("127.0.0.1", port), timeout=30.0) as sock:
        loaded = 0
        while loaded < list_len:
            take = min(batch, list_len - loaded)
            values = [list_value(loaded + idx, payload_size) for idx in range(take)]
            got = request(sock, [b"RPUSH", key, *values])
            want = f":{loaded + take}\r\n".encode()
            if got != want:
                raise RuntimeError(f"unexpected RPUSH reply: got {got!r}, want {want!r}")
            loaded += take
    return time.perf_counter() - start


def dump_loop(port: int, key: bytes, dumps: int, pipeline: int) -> tuple[float, str, int]:
    digest = hashlib.sha256()
    payload_len = None
    remaining = dumps
    start = time.perf_counter()
    with socket.create_connection(("127.0.0.1", port), timeout=60.0) as sock:
        while remaining:
            take = min(pipeline, remaining)
            sock.sendall(encode_command([b"DUMP", key]) * take)
            for _ in range(take):
                payload = bulk_payload(read_frame(sock))
                digest.update(hashlib.sha256(payload).digest())
                if payload_len is None:
                    payload_len = len(payload)
                elif payload_len != len(payload):
                    raise RuntimeError("DUMP payload length changed during loop")
            remaining -= take
    return time.perf_counter() - start, digest.hexdigest(), payload_len or 0


def capture_transcript(port: int, key: bytes) -> bytes:
    commands: list[list[bytes]] = [
        [b"LLEN", key],
        [b"LRANGE", key, b"0", b"4"],
        [b"LRANGE", key, b"-5", b"-1"],
        [b"OBJECT", b"ENCODING", key],
        [b"DUMP", key],
        [b"DEBUG", b"DIGEST-VALUE", key],
        [b"DEBUG", b"DIGEST"],
    ]
    out = bytearray()
    with socket.create_connection(("127.0.0.1", port), timeout=30.0) as sock:
        debug_object = [b"DEBUG", b"OBJECT", key]
        out.extend(encode_command(debug_object))
        out.extend(re.sub(rb"lru:\d+", b"lru:<clock>", request(sock, debug_object)))
        for parts in commands:
            encoded = encode_command(parts)
            out.extend(encoded)
            out.extend(request(sock, parts))
    return bytes(out)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--list-len", type=int, default=10_000)
    parser.add_argument("--payload-size", type=int, default=16)
    parser.add_argument("--load-batch", type=int, default=512)
    parser.add_argument("--dumps", type=int, default=400)
    parser.add_argument("--dump-pipeline", type=int, default=16)
    parser.add_argument("--json-out")
    parser.add_argument("--transcript-out")
    args = parser.parse_args()

    key = b"list:0000"
    with tempfile.TemporaryDirectory(prefix="fr_list_dump_"):
        proc = subprocess.Popen(
            [
                args.server_bin,
                "--bind",
                "127.0.0.1",
                "--port",
                str(args.port),
                "--mode",
                "strict",
                "--enable-debug-command",
                "yes",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            wait_ready(args.port)
            with socket.create_connection(("127.0.0.1", args.port), timeout=2.0) as sock:
                request(sock, [b"FLUSHALL"])
            rss_before = rss_kb(proc.pid)
            load_seconds = load_list(
                args.port,
                key,
                args.list_len,
                args.payload_size,
                args.load_batch,
            )
            rss_after_load = rss_kb(proc.pid)
            dump_seconds, dump_digest, dump_payload_bytes = dump_loop(
                args.port,
                key,
                args.dumps,
                args.dump_pipeline,
            )
            transcript = capture_transcript(args.port, key)
            transcript_sha256 = hashlib.sha256(transcript).hexdigest()
            if args.transcript_out:
                Path(args.transcript_out).write_bytes(transcript)
            result = {
                "server_bin": args.server_bin,
                "port": args.port,
                "list_len": args.list_len,
                "payload_size": args.payload_size,
                "load_batch": args.load_batch,
                "dumps": args.dumps,
                "dump_pipeline": args.dump_pipeline,
                "load_seconds": load_seconds,
                "dump_seconds": dump_seconds,
                "dump_ns_per_op": dump_seconds * 1_000_000_000.0 / args.dumps,
                "dump_digest_sha256": dump_digest,
                "dump_payload_bytes": dump_payload_bytes,
                "rss_before_kb": rss_before,
                "rss_after_load_kb": rss_after_load,
                "rss_delta_kb": rss_after_load - rss_before,
                "transcript_sha256": transcript_sha256,
                "transcript_bytes": len(transcript),
            }
            print(json.dumps(result, sort_keys=True))
            if args.json_out:
                Path(args.json_out).write_text(
                    json.dumps(result, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
        finally:
            request_shutdown(args.port)
            try:
                proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                proc.terminate()
                proc.wait(timeout=2.0)


if __name__ == "__main__":
    main()
