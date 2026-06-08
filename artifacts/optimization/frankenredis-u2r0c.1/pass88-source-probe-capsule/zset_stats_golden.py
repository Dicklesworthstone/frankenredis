#!/usr/bin/env python3
"""Golden reply/stats comparator for the u2r0c.1 zset source-probe pass."""

from __future__ import annotations

import argparse
import hashlib
import json
import socket
from typing import Any


SEED = [
    ("ZADD", "za", "1", "a", "2", "b", "3", "c", "4", "d", "5", "e"),
    ("ZADD", "zb", "1", "a", "2", "b", "3", "c"),
    ("ZADD", "zc", "1", "c", "2", "d", "3", "e", "4", "f"),
    ("SET", "str", "wrong"),
]

CASES = [
    ("zintercard2", ("ZINTERCARD", "2", "za", "zb")),
    ("zinter2", ("ZINTER", "2", "za", "zb")),
    ("zinter3", ("ZINTER", "3", "za", "zb", "zc")),
    ("zdiff2", ("ZDIFF", "2", "za", "zb")),
    ("zdiffstore2", ("ZDIFFSTORE", "d", "2", "za", "zb")),
    ("wrong-type", ("ZDIFF", "2", "za", "str")),
]


class Conn:
    def __init__(self, port: int) -> None:
        self.sock = socket.create_connection(("127.0.0.1", port), 3)
        self.sock.settimeout(30)
        self.buf = b""

    def close(self) -> None:
        self.sock.close()

    def __enter__(self):
        return self

    def __exit__(self, _exc_type, _exc, _tb) -> bool:
        self.close()
        return False

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
    for command in SEED:
        conn.cmd(*command)
    conn.cmd("CONFIG", "RESETSTAT")


def info_stats(conn: Conn) -> dict[str, int]:
    reply = conn.cmd("INFO", "stats")
    info = bytes.fromhex(reply["bulk"]).decode("utf-8", "replace")
    result = {"keyspace_hits": 0, "keyspace_misses": 0}
    for line in info.splitlines():
        if line.startswith("keyspace_hits:"):
            result["keyspace_hits"] = int(line.split(":", 1)[1])
        elif line.startswith("keyspace_misses:"):
            result["keyspace_misses"] = int(line.split(":", 1)[1])
    return result


def transcript(conn: Conn) -> list[dict[str, Any]]:
    output = []
    for label, command in CASES:
        seed(conn)
        reply = conn.cmd(*command)
        output.append({"label": label, "command": command, "reply": reply, "stats": info_stats(conn)})
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
        with Conn(port) as conn:
            outputs[name] = transcript(conn)

    reply_equal = all(
        base["reply"] == cand["reply"] == red["reply"]
        for base, cand, red in zip(outputs["baseline"], outputs["candidate"], outputs["redis"])
    )
    candidate_stats_equal_redis = all(
        cand["stats"] == red["stats"] for cand, red in zip(outputs["candidate"], outputs["redis"])
    )
    payload = json.dumps(outputs, sort_keys=True, separators=(",", ":")).encode()
    digest = hashlib.sha256(payload).hexdigest()
    print(
        json.dumps(
            {
                "candidate_stats_equal_redis": candidate_stats_equal_redis,
                "outputs": outputs,
                "reply_equal": reply_equal,
                "sha256": digest,
            },
            sort_keys=True,
        )
    )
    if not reply_equal or not candidate_stats_equal_redis:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
