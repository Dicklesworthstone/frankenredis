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


def setup_dataset(conn: Conn, width: int) -> dict[str, object]:
    conn.call_raw("FLUSHALL")
    initial = b"a" * width
    reply = conn.call_raw("SET", "s", initial)
    if reply != b"+OK\r\n":
        raise AssertionError(f"unexpected SET reply {reply!r}")
    first = conn.call_raw("SETRANGE", "s", "32", "x")
    return {
        "width": width,
        "first_reply": first.decode("ascii", errors="replace").strip(),
        "first_reply_sha256": hashlib.sha256(first).hexdigest(),
        "strlen": int(conn.call_raw("STRLEN", "s")[1:-2]),
    }


def golden_transcript(conn: Conn, width: int) -> bytes:
    conn.call_raw("FLUSHALL")
    raw = bytearray()
    raw.extend(conn.call_raw("SET", "s", b"a" * width))
    raw.extend(conn.call_raw("SETRANGE", "s", "32", "x"))
    raw.extend(conn.call_raw("GET", "s"))
    raw.extend(conn.call_raw("SETRANGE", "missing", "3", "zz"))
    raw.extend(conn.call_raw("GET", "missing"))
    raw.extend(conn.call_raw("SETRANGE", "empty-missing", "0", b""))
    raw.extend(conn.call_raw("LPUSH", "list", "a"))
    raw.extend(conn.call_raw("SETRANGE", "list", "600000000", "v"))
    raw.extend(conn.call_raw("SETRANGE", "s", "x", "y"))
    raw.extend(conn.call_raw("SETRANGE", "s", "-1", "y"))
    raw.extend(conn.call_raw("SETRANGE", "s", "600000000", "v"))
    raw.extend(conn.call_raw("QUIT"))
    return bytes(raw)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--mode", choices=["setup", "golden"], required=True)
    parser.add_argument("--width", type=int, default=64)
    parser.add_argument("--json-out")
    parser.add_argument("--raw-out")
    args = parser.parse_args()

    conn = Conn(args.port)
    try:
        if args.mode == "setup":
            output = setup_dataset(conn, args.width)
            output["mode"] = "setup"
            output["port"] = args.port
        else:
            raw = golden_transcript(conn, args.width)
            if args.raw_out:
                with open(args.raw_out, "wb") as handle:
                    handle.write(raw)
            output = {
                "mode": "golden",
                "port": args.port,
                "width": args.width,
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
