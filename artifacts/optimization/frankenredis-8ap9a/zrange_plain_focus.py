#!/usr/bin/env python3
import argparse
import hashlib
import json
import socket
import sys


class Conn:
    def __init__(self, port: int):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=30)
        self.buf = b""

    def close(self) -> None:
        self.sock.close()

    def _recv(self) -> None:
        data = self.sock.recv(65536)
        if not data:
            raise EOFError("server closed connection")
        self.buf += data

    def send(self, *args: object) -> None:
        out = b"*%d\r\n" % len(args)
        for arg in args:
            if isinstance(arg, bytes):
                data = arg
            else:
                data = str(arg).encode()
            out += b"$%d\r\n%s\r\n" % (len(data), data)
        self.sock.sendall(out)

    def read_raw(self) -> bytes:
        while b"\r\n" not in self.buf:
            self._recv()
        line, self.buf = self.buf.split(b"\r\n", 1)
        tag = line[:1]
        if tag in (b"+", b"-", b":"):
            return line + b"\r\n"
        if tag == b"$":
            n = int(line[1:])
            if n < 0:
                return line + b"\r\n"
            while len(self.buf) < n + 2:
                self._recv()
            data = self.buf[: n + 2]
            self.buf = self.buf[n + 2 :]
            return line + b"\r\n" + data
        if tag == b"*":
            n = int(line[1:])
            if n < 0:
                return line + b"\r\n"
            parts = [line + b"\r\n"]
            for _ in range(n):
                parts.append(self.read_raw())
            return b"".join(parts)
        raise ValueError(f"unknown RESP tag {tag!r}")

    def call_raw(self, *args: object) -> bytes:
        self.send(*args)
        return self.read_raw()


def pipe(conn: Conn, commands: list[list[object]]) -> list[bytes]:
    for command in commands:
        conn.send(*command)
    return [conn.read_raw() for _ in commands]


def setup_dataset(conn: Conn, members: int) -> dict[str, object]:
    pipe(conn, [["FLUSHALL"]])
    for start in range(0, members, 128):
        args: list[object] = ["ZADD", "z"]
        for i in range(start, min(start + 128, members)):
            args.extend([i, f"m{i:06d}"])
        reply = conn.call_raw(*args)
        expected = b":%d\r\n" % (len(args[2:]) // 2)
        if reply != expected:
            raise AssertionError(f"unexpected ZADD reply {reply!r}, expected {expected!r}")
    return {
        "members": members,
        "zcard": int(conn.call_raw("ZCARD", "z")[1:-2]),
        "first_reply_sha256": hashlib.sha256(conn.call_raw("ZRANGE", "z", "0", "9")).hexdigest(),
    }


def golden_transcript(conn: Conn, members: int) -> bytes:
    setup_dataset(conn, members)
    commands: list[list[object]] = [
        ["ZRANGE", "z", "0", "9"],
        ["ZRANGE", "z", "-10", "-1"],
        ["ZRANGE", "z", "10", "5"],
        ["ZRANGE", "missing", "0", "9"],
        ["SET", "s", "v"],
        ["ZRANGE", "s", "0", "1"],
        ["ZRANGE", "z", "x", "1"],
        ["ZRANGE", "z", "0", "-1", "WITHSCORES"],
        ["QUIT"],
    ]
    raw = bytearray()
    for command in commands:
        raw.extend(conn.call_raw(*command))
    return bytes(raw)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--mode", choices=["setup", "golden"], required=True)
    parser.add_argument("--members", type=int, default=512)
    parser.add_argument("--json-out")
    parser.add_argument("--raw-out")
    args = parser.parse_args()

    conn = Conn(args.port)
    try:
        if args.mode == "setup":
            output = setup_dataset(conn, args.members)
            output["mode"] = "setup"
            output["port"] = args.port
        else:
            raw = golden_transcript(conn, args.members)
            if args.raw_out:
                with open(args.raw_out, "wb") as handle:
                    handle.write(raw)
            output = {
                "mode": "golden",
                "port": args.port,
                "members": args.members,
                "bytes": len(raw),
                "sha256": hashlib.sha256(raw).hexdigest(),
            }
        if args.json_out:
            with open(args.json_out, "w", encoding="utf-8") as handle:
                json.dump(output, handle, indent=2, sort_keys=True)
                handle.write("\n")
        print(json.dumps(output, sort_keys=True))
        return 0
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main())
