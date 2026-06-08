#!/usr/bin/env python3
import argparse
import json
import socket
import statistics
import threading
import time
from pathlib import Path


def resp_command(parts):
    out = bytearray()
    out.extend(f"*{len(parts)}\r\n".encode())
    for part in parts:
        if isinstance(part, str):
            part = part.encode()
        out.extend(f"${len(part)}\r\n".encode())
        out.extend(part)
        out.extend(b"\r\n")
    return bytes(out)


def read_one(buf, start):
    if start >= len(buf):
        return None
    prefix = buf[start]
    if prefix in (43, 45, 58):
        end = buf.find(b"\r\n", start)
        if end < 0:
            return None
        return end + 2
    if prefix == 36:
        end = buf.find(b"\r\n", start)
        if end < 0:
            return None
        length = int(buf[start + 1:end])
        if length < 0:
            return end + 2
        needed = end + 2 + length + 2
        if len(buf) < needed:
            return None
        return needed
    if prefix == 42:
        raise RuntimeError("array replies are not expected in this workload")
    raise RuntimeError(f"unexpected RESP prefix {prefix!r}")


class RespClient:
    def __init__(self, host, port, timeout):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.settimeout(timeout)
        self.buf = bytearray()
        self.start = 0
        self.bytes_sent = 0
        self.bytes_received = 0

    def close(self):
        self.sock.close()

    def request_many(self, commands, expected):
        payload = b"".join(commands)
        self.sock.sendall(payload)
        self.bytes_sent += len(payload)
        seen = 0
        while seen < expected:
            next_start = read_one(self.buf, self.start)
            if next_start is None:
                chunk = self.sock.recv(65536)
                if not chunk:
                    raise RuntimeError("connection closed while reading replies")
                self.buf.extend(chunk)
                self.bytes_received += len(chunk)
                continue
            self.start = next_start
            seen += 1
            if self.start > 1 << 20:
                del self.buf[:self.start]
                self.start = 0


def value_for(index, size):
    if size <= 0:
        return b""
    stem = f"v{index:016x}".encode()
    if len(stem) >= size:
        return stem[:size]
    repeats = (size + len(stem) - 1) // len(stem)
    return (stem * repeats)[:size]


def key_for(prefix, index):
    return f"{prefix}:{index:010d}".encode()


def command_for(mode, prefix, index, datasize):
    key = key_for(prefix, index)
    value = value_for(index, datasize)
    if mode in ("setnx-hit", "setnx-miss"):
        return resp_command((b"SETNX", key, value))
    if mode == "getset-hit":
        return resp_command((b"GETSET", key, value))
    if mode == "getdel-hit":
        return resp_command((b"GETDEL", key))
    if mode == "setex":
        return resp_command((b"SETEX", key, b"600", value))
    if mode == "psetex":
        return resp_command((b"PSETEX", key, b"600000", value))
    if mode == "append":
        return resp_command((b"APPEND", key, value))
    if mode == "set":
        return resp_command((b"SET", key, value))
    raise ValueError(mode)


def prefill_command(mode, prefix, index, datasize):
    key = key_for(prefix, index)
    value = value_for(index, datasize)
    if mode in ("setnx-hit", "getset-hit", "getdel-hit", "append"):
        return resp_command((b"SET", key, value))
    return None


def run_prefill(args, count):
    if count <= 0:
        return {"commands": 0, "seconds": 0.0, "bytes_sent": 0, "bytes_received": 0}
    client = RespClient(args.host, args.port, args.timeout)
    sent = 0
    received = 0
    completed = 0
    start = time.perf_counter()
    try:
        batch = []
        for index in range(count):
            cmd = prefill_command(args.mode, args.key_prefix, index, args.datasize)
            if cmd is None:
                continue
            batch.append(cmd)
            if len(batch) == args.pipeline:
                client.request_many(batch, len(batch))
                completed += len(batch)
                batch.clear()
        if batch:
            client.request_many(batch, len(batch))
            completed += len(batch)
        sent = client.bytes_sent
        received = client.bytes_received
    finally:
        client.close()
    return {
        "commands": completed,
        "seconds": time.perf_counter() - start,
        "bytes_sent": sent,
        "bytes_received": received,
    }


def measured_index(mode, request_index, keyspace):
    if mode in ("setnx-miss", "getdel-hit"):
        return request_index
    return request_index % keyspace


def worker(args, worker_index, start_index, count, results):
    client = RespClient(args.host, args.port, args.timeout)
    latencies = []
    completed = 0
    try:
        batch = []
        batch_start = time.perf_counter()
        for offset in range(count):
            global_index = start_index + offset
            index = measured_index(args.mode, global_index, args.keyspace)
            batch.append(command_for(args.mode, args.key_prefix, index, args.datasize))
            if len(batch) == args.pipeline:
                t0 = time.perf_counter()
                client.request_many(batch, len(batch))
                latencies.append((time.perf_counter() - t0) * 1_000_000.0)
                completed += len(batch)
                batch.clear()
                batch_start = time.perf_counter()
        if batch:
            client.request_many(batch, len(batch))
            latencies.append((time.perf_counter() - batch_start) * 1_000_000.0)
            completed += len(batch)
    finally:
        results[worker_index] = {
            "completed": completed,
            "latencies_us": latencies,
            "bytes_sent": client.bytes_sent,
            "bytes_received": client.bytes_received,
        }
        client.close()


def percentile(values, quantile):
    if not values:
        return 0.0
    ordered = sorted(values)
    pos = min(len(ordered) - 1, int((len(ordered) - 1) * quantile))
    return ordered[pos]


def run_measured(args):
    per_client = args.requests // args.clients
    remainder = args.requests % args.clients
    results = [None] * args.clients
    threads = []
    next_index = 0
    start = time.perf_counter()
    for client_index in range(args.clients):
        count = per_client + (1 if client_index < remainder else 0)
        thread = threading.Thread(
            target=worker,
            args=(args, client_index, next_index, count, results),
        )
        thread.start()
        threads.append(thread)
        next_index += count
    for thread in threads:
        thread.join()
    elapsed = time.perf_counter() - start
    completed = sum(result["completed"] for result in results)
    latencies = [x for result in results for x in result["latencies_us"]]
    bytes_sent = sum(result["bytes_sent"] for result in results)
    bytes_received = sum(result["bytes_received"] for result in results)
    return {
        "commands": completed,
        "seconds": elapsed,
        "ops_per_sec": completed / elapsed if elapsed else 0.0,
        "pipeline_latency_us": {
            "p50": percentile(latencies, 0.50),
            "p95": percentile(latencies, 0.95),
            "p99": percentile(latencies, 0.99),
            "mean": statistics.fmean(latencies) if latencies else 0.0,
            "samples": len(latencies),
        },
        "bytes_sent": bytes_sent,
        "bytes_received": bytes_received,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--mode", required=True)
    parser.add_argument("--requests", type=int, default=300_000)
    parser.add_argument("--clients", type=int, default=50)
    parser.add_argument("--pipeline", type=int, default=16)
    parser.add_argument("--keyspace", type=int, default=10_000)
    parser.add_argument("--datasize", type=int, default=3)
    parser.add_argument("--key-prefix", default="fr6tsou")
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("--phase", choices=("both", "prefill", "run"), default="both")
    parser.add_argument("--json-out")
    args = parser.parse_args()

    if args.mode in ("setnx-miss", "getdel-hit") and args.keyspace < args.requests:
        args.keyspace = args.requests
    prefill_count = args.keyspace if args.phase in ("both", "prefill") else 0
    if args.mode in ("setnx-miss", "setex", "psetex", "set"):
        prefill_count = 0

    report = {
        "schema": "frankenredis_6tsou_resp_workload_v1",
        "mode": args.mode,
        "requests": args.requests,
        "clients": args.clients,
        "pipeline": args.pipeline,
        "keyspace": args.keyspace,
        "datasize": args.datasize,
        "key_prefix": args.key_prefix,
        "prefill": run_prefill(args, prefill_count) if args.phase in ("both", "prefill") else None,
        "measured": None,
    }
    if args.phase in ("both", "run"):
        report["measured"] = run_measured(args)

    text = json.dumps(report, sort_keys=True, indent=2)
    print(text)
    if args.json_out:
        Path(args.json_out).write_text(text + "\n")


if __name__ == "__main__":
    main()
