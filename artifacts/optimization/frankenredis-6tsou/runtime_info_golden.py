#!/usr/bin/env python3
"""Golden transcript for the runtime-info refresh optimization."""

import argparse
import hashlib
import socket
import subprocess
import sys
import time
from pathlib import Path


def wait_for_port(port: int, timeout_s: float) -> None:
    deadline = time.monotonic() + timeout_s
    last_error: OSError | None = None
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError as exc:
            last_error = exc
            time.sleep(0.02)
    raise RuntimeError(f"server did not open port {port}: {last_error}")


def resp_command(parts: list[bytes | str]) -> bytes:
    out = bytearray()
    out.extend(f"*{len(parts)}\r\n".encode())
    for part in parts:
        if isinstance(part, str):
            part = part.encode()
        out.extend(f"${len(part)}\r\n".encode())
        out.extend(part)
        out.extend(b"\r\n")
    return bytes(out)


def read_exact(sock: socket.socket, length: int) -> bytes:
    out = bytearray()
    while len(out) < length:
        chunk = sock.recv(length - len(out))
        if not chunk:
            raise RuntimeError("connection closed while reading reply")
        out.extend(chunk)
    return bytes(out)


def read_line(sock: socket.socket) -> bytes:
    out = bytearray()
    while not out.endswith(b"\r\n"):
        chunk = sock.recv(1)
        if not chunk:
            raise RuntimeError("connection closed while reading line")
        out.extend(chunk)
    return bytes(out)


def read_reply(sock: socket.socket) -> bytes:
    line = read_line(sock)
    prefix = line[:1]
    if prefix in (b"+", b"-", b":", b",", b"_"):
        return line
    if prefix == b"$":
        length = int(line[1:-2])
        if length < 0:
            return line
        return line + read_exact(sock, length + 2)
    if prefix == b"*":
        count = int(line[1:-2])
        out = bytearray(line)
        for _ in range(max(count, 0)):
            out.extend(read_reply(sock))
        return bytes(out)
    if prefix == b"%":
        count = int(line[1:-2])
        out = bytearray(line)
        for _ in range(max(count, 0) * 2):
            out.extend(read_reply(sock))
        return bytes(out)
    if prefix == b"=":
        length = int(line[1:-2])
        return line + read_exact(sock, length + 2)
    raise RuntimeError(f"unexpected RESP prefix {prefix!r}: {line!r}")


SCRIPT: list[list[bytes | str]] = [
    ["FLUSHALL"],
    ["SET", "fr:refresh:k", "v1"],
    ["GETSET", "fr:refresh:k", "v2"],
    ["CLIENT", "TRACKING", "ON", "BCAST"],
    ["INFO", "clients", "persistence"],
    ["CLIENT", "TRACKINGINFO"],
]


def capture(server_bin: str, port: int, transcript_path: Path) -> str:
    server = subprocess.Popen(
        [server_bin, "--bind", "127.0.0.1", "--port", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        wait_for_port(port, 5.0)
        with socket.create_connection(("127.0.0.1", port), timeout=3.0) as sock:
            chunks: list[bytes] = []
            for command in SCRIPT:
                payload = resp_command(command)
                sock.sendall(payload)
                reply = read_reply(sock)
                rendered = b" ".join(
                    part if isinstance(part, bytes) else part.encode() for part in command
                )
                chunks.append(rendered + b"\n" + reply + b"\n")
        transcript = b"".join(chunks)
        transcript_path.write_bytes(transcript)
        return hashlib.sha256(transcript).hexdigest()
    finally:
        server.terminate()
        try:
            server.wait(timeout=3.0)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=3.0)
        if server.returncode not in (0, -15) and server.stderr is not None:
            err = server.stderr.read()
            if err:
                print(err, file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-bin", required=True)
    parser.add_argument("--candidate-bin", required=True)
    parser.add_argument("--baseline-port", type=int, required=True)
    parser.add_argument("--candidate-port", type=int, required=True)
    parser.add_argument("--out-dir", required=True)
    args = parser.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    baseline_sha = capture(
        args.baseline_bin,
        args.baseline_port,
        out_dir / "runtime-info-baseline.transcript",
    )
    candidate_sha = capture(
        args.candidate_bin,
        args.candidate_port,
        out_dir / "runtime-info-candidate.transcript",
    )
    (out_dir / "runtime-info-sha256.txt").write_text(
        f"{baseline_sha}  runtime-info-baseline.transcript\n"
        f"{candidate_sha}  runtime-info-candidate.transcript\n",
        encoding="utf-8",
    )
    print(f"baseline  sha256 = {baseline_sha}")
    print(f"candidate sha256 = {candidate_sha}")
    print(f"ISOMORPHISM (candidate==baseline): {baseline_sha == candidate_sha}")
    return 0 if baseline_sha == candidate_sha else 1


if __name__ == "__main__":
    raise SystemExit(main())
