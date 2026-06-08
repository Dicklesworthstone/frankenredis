#!/usr/bin/env python3
"""Isomorphism + golden proof for the q0qym APPEND borrowed write fast-path.
Runs an identical APPEND transcript against three servers — the candidate
(fast-path) binary, the baseline (generic-path, stashed) binary, and the redis
7.2.4 oracle — and asserts byte-identical reply streams (candidate == baseline ==
oracle). candidate==baseline is the isomorphism proof; ==oracle is parity.

Usage: append_fastpath_golden.py <oracle_port> <candidate_port> <baseline_port>
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

# Deterministic transcript: create, grow, empty-append, wrong-type, binary key
# + value, and the disabling conditions (MULTI/EXEC, SELECT 1).
SCRIPT = [
    ["FLUSHALL"],
    ["APPEND", "c", "Hello"],            # create -> 5
    ["APPEND", "c", " World"],           # grow   -> 11
    ["APPEND", "c", ""],                 # no-op  -> 11
    ["GET", "c"],
    ["SET", "n", "42"],
    ["APPEND", "n", "x"],                # numeric string is still a string -> 3
    ["GET", "n"],
    ["RPUSH", "lst", "x"],
    ["APPEND", "lst", "y"],              # WRONGTYPE
    ["APPEND", "\x00\x01bin", "\x00\xffval"],  # binary key + value
    ["GET", "\x00\x01bin"],
    ["APPEND", "fresh", ""],             # create-empty -> 0
    ["GET", "fresh"],
    # disabling conditions: must fall back and still match
    ["SELECT", "1"],
    ["APPEND", "c", "db1"],              # db 1 -> 3
    ["SELECT", "0"],
    ["MULTI"],
    ["APPEND", "c", "Q"],                # queued
    ["EXEC"],
    ["GET", "c"],
    ["STRLEN", "c"],
]

def transcript(port):
    s = conn(port)
    out = b""
    for cmd in SCRIPT:
        out += cmd[0].encode() + b" => " + send(s, *cmd) + b"\n"
    return out

oracle, cand, base = (int(x) for x in sys.argv[1:4])
o = transcript(oracle); c = transcript(cand); b = transcript(base)
ho, hc, hb = (hashlib.sha256(x).hexdigest() for x in (o, c, b))
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
            print(f"  first diff @ line {i}:\n    oracle   ={x!r}\n    candidate={y!r}\n    baseline ={z!r}")
            break
sys.exit(0 if (iso and parity) else 1)
