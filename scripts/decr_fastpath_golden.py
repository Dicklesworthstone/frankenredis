#!/usr/bin/env python3
"""Isomorphism + golden proof for the q0qym DECR/DECRBY borrowed write fast-path.
Runs an identical DECR/DECRBY transcript against three servers — the candidate
(fast-path) binary, the baseline (generic-path, stashed) binary, and the redis
7.2.4 oracle — and asserts byte-identical reply streams (candidate == baseline ==
oracle). The candidate==baseline equality is the isomorphism proof (fast path
matches the generic path); ==oracle is the parity proof.

Usage: decr_fastpath_golden.py <oracle_port> <candidate_port> <baseline_port>
"""
import socket, sys, hashlib

def conn(p):
    s = socket.create_connection(("127.0.0.1", p)); s.settimeout(3); return s

def send(s, *args):
    b = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str): a = a.encode()
        b += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(b)
    return read_reply(s)

def read_reply(s):
    line = b""
    while not line.endswith(b"\r\n"):
        ch = s.recv(1)
        if not ch: break
        line += ch
    t = line[:1]
    if t in (b"+", b"-", b":"):
        return line
    if t == b"$":
        n = int(line[1:-2])
        if n == -1: return line
        data = b""
        while len(data) < n + 2:
            data += s.recv(n + 2 - len(data))
        return line + data
    if t == b"*":
        n = int(line[1:-2])
        out = line
        for _ in range(max(n, 0)):
            out += read_reply(s)
        return out
    return line

# Deterministic transcript exercising the DECR/DECRBY fast path and every
# fall-back: create/decrement, negative delta (=increment), wrong-type,
# non-integer value, overflow, LLONG_MIN delta (deferred to generic), binary
# key, and the disabling conditions (MULTI/EXEC, SELECT 1).
SCRIPT = [
    ["FLUSHALL"],
    ["DECR", "c"],                       # missing -> -1
    ["DECR", "c"],                       # -2
    ["DECRBY", "c", "5"],                # -7
    ["DECRBY", "c", "-3"],               # -4 (negative delta increments)
    ["GET", "c"],
    ["SET", "n", "notanumber"],
    ["DECR", "n"],                       # not an integer
    ["DECRBY", "n", "2"],                # not an integer
    ["RPUSH", "lst", "x"],
    ["DECR", "lst"],                     # WRONGTYPE
    ["DECRBY", "lst", "2"],              # WRONGTYPE
    ["SET", "min", "-9223372036854775808"],
    ["DECR", "min"],                     # overflow
    ["SET", "z", "0"],
    ["DECRBY", "z", "-9223372036854775808"],  # LLONG_MIN delta -> decrement would overflow
    ["DECRBY", "z", "9223372036854775807"],   # large delta ok
    ["DECR", "\x00\x01bin"],             # binary key -> -1
    ["DECRBY", "\x00\x01bin", "10"],     # -11
    ["GET", "\x00\x01bin"],
    # disabling conditions: fast path must fall back and still match
    ["SELECT", "1"],
    ["DECR", "c"],                       # db 1 -> -1
    ["SELECT", "0"],
    ["MULTI"],
    ["DECR", "c"],                       # queued
    ["DECRBY", "c", "2"],                # queued
    ["EXEC"],
    ["GET", "c"],
    ["DECRBY", "c", "abc"],              # non-integer delta -> deferred
]

def transcript(port):
    s = conn(port)
    out = b""
    for cmd in SCRIPT:
        out += cmd[0].encode() + b" => " + send(s, *cmd) + b"\n"
    return out

oracle, cand, base = (int(x) for x in sys.argv[1:4])
o = transcript(oracle); c = transcript(cand); b = transcript(base)
ho = hashlib.sha256(o).hexdigest()
hc = hashlib.sha256(c).hexdigest()
hb = hashlib.sha256(b).hexdigest()
print(f"oracle    sha256 = {ho}  ({len(o)} bytes)")
print(f"candidate sha256 = {hc}")
print(f"baseline  sha256 = {hb}")
iso = hc == hb
parity = hc == ho
print(f"ISOMORPHISM (candidate==baseline): {iso}")
print(f"PARITY      (candidate==oracle):   {parity}")
if not (iso and parity):
    for i, (x, y, z) in enumerate(zip(o.split(b"\n"), c.split(b"\n"), b.split(b"\n"))):
        if not (x == y == z):
            print(f"  first diff @ line {i}:\n    oracle  ={x!r}\n    candidate={y!r}\n    baseline ={z!r}")
            break
sys.exit(0 if (iso and parity) else 1)
