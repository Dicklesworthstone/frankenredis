#!/usr/bin/env python3
"""Compare exact HSET-family RESP transcripts from two FrankenRedis binaries."""

import argparse
import hashlib
import json
import socket
import subprocess
import time
from pathlib import Path


COMMANDS = [
    (b"FLUSHALL",),
    (b"HSET", b"ohsk5:hash", b"field:1", b"value:1"),
    (b"HSET", b"ohsk5:hash", b"field:2", b"value:2"),
    (b"HSET", b"ohsk5:hash", b"field:1", b"value:1b"),
    (b"HGET", b"ohsk5:hash", b"field:1"),
    (b"HGET", b"ohsk5:hash", b"field:2"),
    (b"HGETALL", b"ohsk5:hash"),
    (b"HLEN", b"ohsk5:hash"),
    (b"HEXISTS", b"ohsk5:hash", b"field:1"),
    (b"HSETNX", b"ohsk5:hash", b"field:1", b"blocked"),
    (b"HSETNX", b"ohsk5:hash", b"field:3", b"value:3"),
    (b"HGETALL", b"ohsk5:hash"),
    (b"HDEL", b"ohsk5:hash", b"field:2"),
    (b"HGETALL", b"ohsk5:hash"),
    (b"OBJECT", b"ENCODING", b"ohsk5:hash"),
    (b"DEBUG", b"DIGEST"),
    (b"SET", b"ohsk5:string", b"plain"),
    (b"HSET", b"ohsk5:string", b"field", b"value"),
    (b"HGETALL", b"ohsk5:missing"),
]


def encode_command(parts):
    out = [b"*" + str(len(parts)).encode("ascii") + b"\r\n"]
    for part in parts:
        out.append(b"$" + str(len(part)).encode("ascii") + b"\r\n")
        out.append(part + b"\r\n")
    return b"".join(out)


def read_exact(sock, size):
    chunks = []
    remaining = size
    while remaining:
        chunk = sock.recv(remaining)
        if not chunk:
            raise EOFError("server closed connection")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_line(sock):
    chunks = []
    while True:
        byte = read_exact(sock, 1)
        chunks.append(byte)
        if len(chunks) >= 2 and chunks[-2:] == [b"\r", b"\n"]:
            return b"".join(chunks)


def read_frame(sock):
    first = read_exact(sock, 1)
    if first in (b"+", b"-", b":"):
        return first + read_line(sock)
    if first == b"$":
        line = read_line(sock)
        length = int(line[:-2])
        if length == -1:
            return first + line
        return first + line + read_exact(sock, length + 2)
    if first == b"*":
        line = read_line(sock)
        count = int(line[:-2])
        out = [first, line]
        if count == -1:
            return b"".join(out)
        for _ in range(count):
            out.append(read_frame(sock))
        return b"".join(out)
    raise ValueError(f"unsupported RESP type byte: {first!r}")


def wait_for_port(port, timeout_s=5.0):
    deadline = time.monotonic() + timeout_s
    last_error = None
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError as exc:
            last_error = exc
            time.sleep(0.02)
    raise RuntimeError(f"server did not open port {port}: {last_error}")


def run_transcript(server_bin, port):
    with subprocess.Popen(
        [server_bin, "--bind", "127.0.0.1", "--port", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        cwd=Path(__file__).resolve().parent,
        text=True,
    ) as server:
        try:
            wait_for_port(port)
            transcript = bytearray()
            with socket.create_connection(("127.0.0.1", port), timeout=5.0) as sock:
                for command in COMMANDS:
                    wire = encode_command(command)
                    sock.sendall(wire)
                    reply = read_frame(sock)
                    transcript.extend(b"> ")
                    transcript.extend(b" ".join(command))
                    transcript.extend(b"\n< ")
                    transcript.extend(reply)
                    transcript.extend(b"\n")
            return bytes(transcript)
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
                    raise RuntimeError(err)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-bin", required=True)
    parser.add_argument("--candidate-bin", required=True)
    parser.add_argument("--baseline-port", type=int, default=48931)
    parser.add_argument("--candidate-port", type=int, default=48932)
    parser.add_argument("--json-out", required=True)
    parser.add_argument("--baseline-transcript-out", required=True)
    parser.add_argument("--candidate-transcript-out", required=True)
    args = parser.parse_args()

    baseline = run_transcript(args.baseline_bin, args.baseline_port)
    candidate = run_transcript(args.candidate_bin, args.candidate_port)
    baseline_sha = sha256(baseline)
    candidate_sha = sha256(candidate)

    Path(args.baseline_transcript_out).write_bytes(baseline)
    Path(args.candidate_transcript_out).write_bytes(candidate)
    result = {
        "baseline_sha256": baseline_sha,
        "candidate_sha256": candidate_sha,
        "equal": baseline == candidate,
        "commands": [[part.decode("ascii") for part in command] for command in COMMANDS],
        "proof_obligations": {
            "hset_new_vs_overwrite_counts": "raw integer replies are byte-compared",
            "hash_field_order": "HGETALL transcripts are byte-compared",
            "encoding_promotion": "OBJECT ENCODING reply is byte-compared",
            "global_digest": "DEBUG DIGEST reply is byte-compared",
            "wrongtype_ordering": "string-key HSET error reply is byte-compared",
        },
    }
    Path(args.json_out).write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    if baseline != candidate:
        return 1
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
