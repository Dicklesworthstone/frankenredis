#!/usr/bin/env python3
"""Golden comparator for the frankenredis-1jhwe ZDIFFSTORE pass."""

from __future__ import annotations

import argparse
import hashlib
import json
import socket
from typing import Any


class Conn:
    def __init__(self, port: int) -> None:
        self.sock = socket.create_connection(("127.0.0.1", port), 3)
        self.sock.settimeout(30)
        self.buf = b""

    def close(self) -> None:
        self.sock.close()

    def _line(self) -> bytes:
        while b"\r\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise OSError("connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def _bytes(self, count: int) -> bytes:
        while len(self.buf) < count + 2:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise OSError("connection closed")
            self.buf += chunk
        data, self.buf = self.buf[:count], self.buf[count + 2 :]
        return data

    def parse(self) -> Any:
        line = self._line()
        tag, rest = line[:1], line[1:]
        if tag == b"$":
            count = int(rest)
            return {"bulk": None} if count < 0 else {"bulk": self._bytes(count).hex()}
        if tag == b":":
            return {"int": int(rest)}
        if tag == b"+":
            return {"status": rest.decode("utf-8", "replace")}
        if tag == b"-":
            return {"error": rest.decode("utf-8", "replace")}
        if tag == b"*":
            count = int(rest)
            return {"array": None} if count < 0 else {"array": [self.parse() for _ in range(count)]}
        raise ValueError(f"unknown RESP tag {tag!r}")

    def cmd(self, *args: object) -> Any:
        parts = [f"*{len(args)}\r\n".encode()]
        for arg in args:
            data = arg if isinstance(arg, bytes) else str(arg).encode()
            parts.append(b"$%d\r\n%s\r\n" % (len(data), data))
        self.sock.sendall(b"".join(parts))
        return self.parse()


def seed(conn: Conn) -> None:
    conn.cmd("FLUSHALL")
    conn.cmd("ZADD", "z", "1", "a", "2", "b", "3", "c")
    conn.cmd("ZADD", "other", "10", "a")
    conn.cmd("ZADD", "allgone", "1", "a", "2", "b")
    conn.cmd("ZADD", "allgone2", "1", "a", "2", "b")
    conn.cmd("ZADD", "zs", "1", "a", "2", "b", "3", "c")
    conn.cmd("SADD", "s", "a", "c")
    conn.cmd("ZADD", "za", "1", "a", "2", "b", "3", "c")
    conn.cmd("ZADD", "zb", "2", "b", "3", "c", "4", "d")
    conn.cmd("SET", "str", "wrong")


def transcript(conn: Conn) -> list[dict[str, Any]]:
    seed(conn)
    cases: list[tuple[str, tuple[object, ...], tuple[object, ...] | None]] = [
        (
            "dest-source",
            ("ZDIFFSTORE", "z", "2", "z", "other"),
            ("ZRANGE", "z", "0", "-1", "WITHSCORES"),
        ),
        (
            "empty-removes-dest",
            ("ZDIFFSTORE", "allgone", "2", "allgone", "allgone2"),
            ("EXISTS", "allgone"),
        ),
        (
            "set-source",
            ("ZDIFFSTORE", "ds", "2", "zs", "s"),
            ("ZRANGE", "ds", "0", "-1", "WITHSCORES"),
        ),
        (
            "missing-source",
            ("ZDIFFSTORE", "dm", "2", "zs", "missing"),
            ("ZRANGE", "dm", "0", "-1", "WITHSCORES"),
        ),
        (
            "wrong-type",
            ("ZDIFFSTORE", "dw", "2", "zs", "str"),
            None,
        ),
        (
            "zintercard-guard",
            ("ZINTERCARD", "2", "za", "zb", "LIMIT", "1"),
            None,
        ),
    ]
    output = []
    for label, command, verify in cases:
        reply = conn.cmd(*command)
        verify_reply = conn.cmd(*verify) if verify is not None else None
        output.append(
            {
                "label": label,
                "command": command,
                "reply": reply,
                "verify": verify,
                "verify_reply": verify_reply,
            }
        )
    return output


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-port", type=int, required=True)
    parser.add_argument("--candidate-port", type=int, required=True)
    parser.add_argument("--redis-port", type=int, required=True)
    args = parser.parse_args()
    outputs = {}
    for name, port in {
        "baseline": args.baseline_port,
        "candidate": args.candidate_port,
        "redis": args.redis_port,
    }.items():
        conn = Conn(port)
        try:
            outputs[name] = transcript(conn)
        finally:
            conn.close()
    payload = json.dumps(outputs, sort_keys=True, separators=(",", ":")).encode()
    digest = hashlib.sha256(payload).hexdigest()
    equal = outputs["baseline"] == outputs["candidate"] == outputs["redis"]
    print(json.dumps({"sha256": digest, "equal": equal, "outputs": outputs}, sort_keys=True))
    if not equal:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
