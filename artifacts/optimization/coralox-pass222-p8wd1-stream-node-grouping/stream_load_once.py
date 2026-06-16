from __future__ import annotations

import argparse
import hashlib
import json
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


def rss_kb(pid: int) -> int:
    with Path(f"/proc/{pid}/status").open("r", encoding="utf-8") as status:
        for line in status:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    raise RuntimeError(f"VmRSS missing for pid {pid}")


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


def stream_key(stream_idx: int) -> bytes:
    return f"stream:{stream_idx:04d}".encode()


def xadd_parts(stream_idx: int, entry_idx: int) -> list[bytes]:
    return [
        b"XADD",
        stream_key(stream_idx),
        f"{entry_idx + 1}-0".encode(),
        b"user_id",
        f"user:{stream_idx % 256:03d}".encode(),
        b"event",
        b"click",
        b"shard",
        f"{stream_idx % 16}".encode(),
        b"ts",
        f"{entry_idx:08d}".encode(),
    ]


def load_streams(port: int, streams: int, entries_per_stream: int, pipeline: int) -> float:
    total = streams * entries_per_stream
    start = time.perf_counter()
    with socket.create_connection(("127.0.0.1", port), timeout=10.0) as sock:
        sent = 0
        while sent < total:
            batch = min(pipeline, total - sent)
            payload = bytearray()
            expected: list[bytes] = []
            for offset in range(batch):
                ordinal = sent + offset
                stream_idx = ordinal // entries_per_stream
                entry_idx = ordinal % entries_per_stream
                payload.extend(encode_command(xadd_parts(stream_idx, entry_idx)))
                expected.append(f"${len(f'{entry_idx + 1}-0')}\r\n{entry_idx + 1}-0\r\n".encode())
            sock.sendall(payload)
            for want in expected:
                got = read_frame(sock)
                if got != want:
                    raise RuntimeError(f"unexpected XADD reply: got {got!r}, want {want!r}")
            sent += batch
    return time.perf_counter() - start


def capture_transcript(port: int, streams: int) -> bytes:
    commands: list[list[bytes]] = [
        [b"XLEN", stream_key(0)],
        [b"XLEN", stream_key(streams - 1)],
        [b"XRANGE", stream_key(0), b"1-0", b"3-0"],
        [b"XREVRANGE", stream_key(streams - 1), b"+", b"-", b"COUNT", b"3"],
        [b"XINFO", b"STREAM", stream_key(0)],
        [b"DUMP", stream_key(0)],
        [b"DEBUG", b"DIGEST-VALUE", stream_key(0)],
        [b"DEBUG", b"DIGEST"],
    ]
    out = bytearray()
    with socket.create_connection(("127.0.0.1", port), timeout=10.0) as sock:
        for parts in commands:
            frame = encode_command(parts)
            out.extend(frame)
            out.extend(request(sock, parts))
    return bytes(out)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--streams", type=int, default=100)
    parser.add_argument("--entries-per-stream", type=int, default=1000)
    parser.add_argument("--pipeline", type=int, default=128)
    parser.add_argument("--json-out")
    parser.add_argument("--transcript-out")
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="fr_stream_rss_"):
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
            before = rss_kb(proc.pid)
            load_seconds = load_streams(
                args.port,
                args.streams,
                args.entries_per_stream,
                args.pipeline,
            )
            after = rss_kb(proc.pid)
            transcript = capture_transcript(args.port, args.streams)
            transcript_sha256 = hashlib.sha256(transcript).hexdigest()
            if args.transcript_out:
                Path(args.transcript_out).write_bytes(transcript)
            entries = args.streams * args.entries_per_stream
            result = {
                "server_bin": args.server_bin,
                "port": args.port,
                "streams": args.streams,
                "entries_per_stream": args.entries_per_stream,
                "entries": entries,
                "pipeline": args.pipeline,
                "rss_before_kb": before,
                "rss_after_kb": after,
                "rss_delta_kb": after - before,
                "bytes_per_entry": ((after - before) * 1024) / entries,
                "load_seconds": load_seconds,
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
