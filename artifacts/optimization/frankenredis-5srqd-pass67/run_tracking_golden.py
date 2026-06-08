#!/usr/bin/env python3
"""Golden transcript for CLIENT TRACKING INFO counters."""

import argparse
import hashlib
import json
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


def command(*parts: bytes) -> bytes:
    out = bytearray(f"*{len(parts)}\r\n".encode())
    for part in parts:
        out.extend(f"${len(part)}\r\n".encode())
        out.extend(part)
        out.extend(b"\r\n")
    return bytes(out)


def read_resp(sock: socket.socket) -> bytes:
    prefix = sock.recv(1)
    if not prefix:
        raise EOFError("socket closed while reading prefix")
    if prefix in (b"+", b"-", b":"):
        return prefix + read_line(sock)
    if prefix == b"$":
        line = read_line(sock)
        length = int(line[:-2])
        if length < 0:
            return prefix + line
        return prefix + line + read_exact(sock, length + 2)
    if prefix == b"*":
        line = read_line(sock)
        count = int(line[:-2])
        payload = bytearray(prefix + line)
        for _ in range(count):
            payload.extend(read_resp(sock))
        return bytes(payload)
    raise ValueError(f"unsupported RESP prefix {prefix!r}")


def read_line(sock: socket.socket) -> bytes:
    data = bytearray()
    while not data.endswith(b"\r\n"):
        chunk = sock.recv(1)
        if not chunk:
            raise EOFError("socket closed while reading line")
        data.extend(chunk)
    return bytes(data)


def read_exact(sock: socket.socket, size: int) -> bytes:
    data = bytearray()
    while len(data) < size:
        chunk = sock.recv(size - len(data))
        if not chunk:
            raise EOFError("socket closed while reading bulk body")
        data.extend(chunk)
    return bytes(data)


def send(sock: socket.socket, *parts: bytes) -> bytes:
    sock.sendall(command(*parts))
    return read_resp(sock)


def bulk_body(frame: bytes) -> str:
    if not frame.startswith(b"$"):
        raise ValueError(f"expected bulk string, got {frame!r}")
    header, body = frame.split(b"\r\n", 1)
    length = int(header[1:])
    return body[:length].decode()


def info_value(body: str, name: str) -> str:
    needle = f"{name}:"
    for line in body.splitlines():
        if line.startswith(needle):
            return line[len(needle) :]
    raise KeyError(name)


def run_once(server_bin: str, port: int) -> dict[str, object]:
    server = subprocess.Popen(
        [server_bin, "--bind", "127.0.0.1", "--port", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        cwd=Path(__file__).resolve().parent,
    )
    try:
        wait_for_port(port, 5.0)
        with socket.create_connection(("127.0.0.1", port), timeout=2.0) as tracker:
            with socket.create_connection(("127.0.0.1", port), timeout=2.0) as observer:
                transcript: dict[str, object] = {}
                transcript["tracker_on"] = send(
                    tracker,
                    b"CLIENT",
                    b"TRACKING",
                    b"ON",
                    b"BCAST",
                    b"PREFIX",
                    b"alpha:",
                    b"PREFIX",
                    b"beta:",
                ).decode()
                clients_info = bulk_body(send(observer, b"INFO", b"clients"))
                stats_info = bulk_body(send(observer, b"INFO", b"stats"))
                transcript["peer_tracking"] = {
                    "tracking_clients": info_value(clients_info, "tracking_clients"),
                    "tracking_total_prefixes": info_value(
                        stats_info, "tracking_total_prefixes"
                    ),
                }
                transcript["observer_on"] = send(
                    observer,
                    b"CLIENT",
                    b"TRACKING",
                    b"ON",
                    b"BCAST",
                    b"PREFIX",
                    b"self:",
                ).decode()
                clients_info = bulk_body(send(observer, b"INFO", b"clients"))
                stats_info = bulk_body(send(observer, b"INFO", b"stats"))
                transcript["current_and_recorded_tracking"] = {
                    "tracking_clients": info_value(clients_info, "tracking_clients"),
                    "tracking_total_prefixes": info_value(
                        stats_info, "tracking_total_prefixes"
                    ),
                }
                transcript["tracker_off"] = send(
                    tracker, b"CLIENT", b"TRACKING", b"OFF"
                ).decode()
                clients_info = bulk_body(send(observer, b"INFO", b"clients"))
                stats_info = bulk_body(send(observer, b"INFO", b"stats"))
                transcript["current_tracking_only"] = {
                    "tracking_clients": info_value(clients_info, "tracking_clients"),
                    "tracking_total_prefixes": info_value(
                        stats_info, "tracking_total_prefixes"
                    ),
                }
                return transcript
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
    parser.add_argument("--baseline-port", type=int, default=6398)
    parser.add_argument("--candidate-port", type=int, default=6399)
    parser.add_argument("--json-out", required=True)
    args = parser.parse_args()

    baseline = run_once(args.baseline_bin, args.baseline_port)
    candidate = run_once(args.candidate_bin, args.candidate_port)
    baseline_bytes = json.dumps(baseline, sort_keys=True, separators=(",", ":")).encode()
    candidate_bytes = json.dumps(candidate, sort_keys=True, separators=(",", ":")).encode()
    report = {
        "baseline": baseline,
        "candidate": candidate,
        "baseline_sha256": hashlib.sha256(baseline_bytes).hexdigest(),
        "candidate_sha256": hashlib.sha256(candidate_bytes).hexdigest(),
        "parity": baseline == candidate,
    }
    Path(args.json_out).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["parity"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
