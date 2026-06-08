#!/usr/bin/env python3
"""Isomorphism + golden proof for a SETNX borrowed write fast path."""

import hashlib
import socket
import sys


def conn(port: int) -> socket.socket:
    sock = socket.create_connection(("127.0.0.1", port))
    sock.settimeout(3)
    return sock


def read_reply(sock: socket.socket) -> bytes:
    line = b""
    while not line.endswith(b"\r\n"):
        chunk = sock.recv(1)
        if not chunk:
            break
        line += chunk
    prefix = line[:1]
    if prefix in (b"+", b"-", b":"):
        return line
    if prefix == b"$":
        length = int(line[1:-2])
        if length == -1:
            return line
        data = b""
        while len(data) < length + 2:
            data += sock.recv(length + 2 - len(data))
        return line + data
    if prefix == b"*":
        count = int(line[1:-2])
        out = line
        for _ in range(max(count, 0)):
            out += read_reply(sock)
        return out
    return line


def send(sock: socket.socket, *args: bytes | str) -> bytes:
    payload = b"*%d\r\n" % len(args)
    for arg in args:
        if isinstance(arg, str):
            arg = arg.encode()
        payload += b"$%d\r\n%s\r\n" % (len(arg), arg)
    sock.sendall(payload)
    return read_reply(sock)


SCRIPT = [
    ["FLUSHALL"],
    ["SETNX", "fresh", "a"],
    ["GET", "fresh"],
    ["SETNX", "fresh", "b"],
    ["GET", "fresh"],
    ["SET", "present", "1"],
    ["SETNX", "present", "2"],
    ["GET", "present"],
    ["RPUSH", "list", "x"],
    ["SETNX", "list", "value"],
    ["TYPE", "list"],
    ["SETNX", "\x00bin", "\x00\xffv"],
    ["GET", "\x00bin"],
    ["SELECT", "1"],
    ["SETNX", "db1", "v"],
    ["GET", "db1"],
    ["SELECT", "0"],
    ["MULTI"],
    ["SETNX", "fresh", "queued"],
    ["EXEC"],
    ["GET", "fresh"],
]


def transcript(port: int) -> bytes:
    sock = conn(port)
    out = b""
    for command in SCRIPT:
        out += command[0].encode() + b" => " + send(sock, *command) + b"\n"
    sock.close()
    return out


def main() -> int:
    oracle, candidate, baseline = (int(arg) for arg in sys.argv[1:4])
    oracle_out = transcript(oracle)
    candidate_out = transcript(candidate)
    baseline_out = transcript(baseline)
    oracle_sha, candidate_sha, baseline_sha = (
        hashlib.sha256(output).hexdigest()
        for output in (oracle_out, candidate_out, baseline_out)
    )
    print(f"oracle    sha256 = {oracle_sha}  ({len(oracle_out)} bytes)")
    print(f"candidate sha256 = {candidate_sha}")
    print(f"baseline  sha256 = {baseline_sha}")
    iso = candidate_sha == baseline_sha
    parity = candidate_sha == oracle_sha
    print(f"ISOMORPHISM (candidate==baseline): {iso}")
    print(f"PARITY      (candidate==oracle):   {parity}")
    if not (iso and parity):
        for index, (oracle_line, candidate_line, baseline_line) in enumerate(
            zip(oracle_out.split(b"\n"), candidate_out.split(b"\n"), baseline_out.split(b"\n"))
        ):
            if not (oracle_line == candidate_line == baseline_line):
                print(
                    "  first diff @ line "
                    f"{index}:\n    oracle   ={oracle_line!r}\n"
                    f"    candidate={candidate_line!r}\n    baseline ={baseline_line!r}"
                )
                break
    return 0 if iso and parity else 1


if __name__ == "__main__":
    raise SystemExit(main())
