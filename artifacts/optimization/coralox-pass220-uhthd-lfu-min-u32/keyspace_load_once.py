from __future__ import annotations

import argparse
import json
import socket
import subprocess
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


def wait_ready(port: int) -> None:
    deadline = time.time() + 10.0
    ping = encode_command([b"PING"])
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2) as sock:
                sock.sendall(ping)
                if read_frame(sock) == b"+PONG\r\n":
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


def load_keyspace(port: int, count: int, pipeline: int) -> float:
    start = time.perf_counter()
    with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
        sent = 0
        while sent < count:
            batch = min(pipeline, count - sent)
            payload = bytearray()
            for offset in range(batch):
                i = sent + offset
                payload.extend(
                    encode_command(
                        [
                            b"SET",
                            f"key:{i:08d}".encode(),
                            f"val:{i:08d}".encode(),
                        ]
                    )
                )
            sock.sendall(payload)
            for _ in range(batch):
                if read_frame(sock) != b"+OK\r\n":
                    raise RuntimeError("SET did not return OK")
            sent += batch
    return time.perf_counter() - start


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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-bin", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--n", type=int, default=1_000_000)
    parser.add_argument("--pipeline", type=int, default=256)
    parser.add_argument("--json-out")
    args = parser.parse_args()

    proc = subprocess.Popen(
        [args.server_bin, "--bind", "127.0.0.1", "--port", str(args.port), "--mode", "strict"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        wait_ready(args.port)
        before = rss_kb(proc.pid)
        load_seconds = load_keyspace(args.port, args.n, args.pipeline)
        after = rss_kb(proc.pid)
        result = {
            "server_bin": args.server_bin,
            "port": args.port,
            "n": args.n,
            "pipeline": args.pipeline,
            "rss_before_kb": before,
            "rss_after_kb": after,
            "rss_delta_kb": after - before,
            "bytes_per_key": ((after - before) * 1024) / args.n,
            "load_seconds": load_seconds,
        }
        print(json.dumps(result, sort_keys=True))
        if args.json_out:
            with Path(args.json_out).open("w", encoding="utf-8") as out:
                out.write(json.dumps(result, indent=2, sort_keys=True) + "\n")
    finally:
        request_shutdown(args.port)
        try:
            proc.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            proc.terminate()
            proc.wait(timeout=2.0)


if __name__ == "__main__":
    main()
