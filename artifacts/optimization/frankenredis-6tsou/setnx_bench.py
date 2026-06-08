#!/usr/bin/env python3
"""Pipelined SETNX benchmark driver for frankenredis-6tsou evidence."""

import argparse
import socket
import time


def frame(key: bytes, value: bytes) -> bytes:
    return (
        b"*3\r\n$5\r\nSETNX\r\n$"
        + str(len(key)).encode()
        + b"\r\n"
        + key
        + b"\r\n$"
        + str(len(value)).encode()
        + b"\r\n"
        + value
        + b"\r\n"
    )


def read_integer_batch(sock: socket.socket, count: int) -> None:
    data = bytearray()
    while data.count(b"\r\n") < count:
        chunk = sock.recv(64 * 1024)
        if not chunk:
            raise RuntimeError("connection closed while reading replies")
        data.extend(chunk)
    for line in data.splitlines():
        if line and not line.startswith(b":"):
            raise RuntimeError(f"unexpected reply: {line!r}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--clients", type=int, default=50)
    parser.add_argument("--requests", type=int, default=300_000)
    parser.add_argument("--pipeline", type=int, default=16)
    parser.add_argument("--keyspace", type=int, default=10_000)
    parser.add_argument("--datasize", type=int, default=3)
    parser.add_argument("--key-prefix", default="fr:6tsou:setnx")
    args = parser.parse_args()

    value = b"x" * args.datasize
    sockets = [
        socket.create_connection((args.host, args.port), timeout=5.0)
        for _ in range(args.clients)
    ]
    for sock in sockets:
        sock.settimeout(5.0)

    sent = 0
    start = time.perf_counter()
    while sent < args.requests:
        for client_index, sock in enumerate(sockets):
            if sent >= args.requests:
                break
            batch = min(args.pipeline, args.requests - sent)
            payload = bytearray()
            for offset in range(batch):
                request_index = sent + offset
                key_index = (request_index * 31 + client_index) % args.keyspace
                key = f"{args.key_prefix}:{key_index}".encode()
                payload.extend(frame(key, value))
            sock.sendall(payload)
            read_integer_batch(sock, batch)
            sent += batch
    elapsed = time.perf_counter() - start

    for sock in sockets:
        sock.close()

    ops = args.requests / elapsed
    print(f"requests={args.requests} elapsed={elapsed:.6f}s ops_per_sec={ops:.2f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
