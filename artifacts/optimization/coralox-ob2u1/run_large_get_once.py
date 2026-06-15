#!/usr/bin/env python3
import socket
import sys
import time


def conn(port: int) -> socket.socket:
    sock = socket.create_connection(("127.0.0.1", port), timeout=30)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return sock


def read_exact(sock: socket.socket, byte_count: int) -> None:
    remaining = byte_count
    while remaining:
        chunk = sock.recv(min(1 << 20, remaining))
        if not chunk:
            raise RuntimeError("connection closed before expected reply bytes")
        remaining -= len(chunk)


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: run_large_get_once.py <port> <value_bytes> <pipeline_count>", file=sys.stderr)
        return 2

    port = int(sys.argv[1])
    value_bytes = int(sys.argv[2])
    pipeline_count = int(sys.argv[3])

    sock = conn(port)
    value = b"x" * value_bytes
    sock.sendall(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$%d\r\n%s\r\n" % (value_bytes, value))
    read_exact(sock, 5)

    request = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n" * pipeline_count
    reply_len = len(b"$%d\r\n" % value_bytes) + value_bytes + 2
    start = time.perf_counter()
    sock.sendall(request)
    read_exact(sock, reply_len * pipeline_count)
    elapsed = time.perf_counter() - start
    sock.close()

    print(f"{pipeline_count / elapsed:.3f} ops/s")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
