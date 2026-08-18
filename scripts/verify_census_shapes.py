#!/usr/bin/env python3
"""Verify the new census shapes return SUCCESSFUL replies, not errors.

A mis-specified shape (wrong seed, wrong arity, wrong key name) still yields a perfectly
reproducible instr/op number -- it just measures the error path instead of the command. This
replays each shape's seed and command against a live server and prints the reply, so a `-ERR`
or an unintended nil is visible before the shape is used for a census or a verdict.
"""
import ast
import re
import socket
import subprocess
import sys
import tempfile
import time

S = "/data/tmp/claude-1000/-data-projects-frankenredis/f82d025c-b982-4760-a679-f7e31fe91efe/scratchpad"
NEW = ["bitfield_get", "bitfield_ro_2get", "hscan_zero", "sscan_zero", "zscan_zero",
       "pexpiretime_base", "unwatch_base", "xread_one", "xrevrange_base", "zdiff_2",
       "zinter_2src"]

src = open("/data/projects/frankenredis/scripts/shape_instr_per_op.py").read()
body = re.search(r"^SHAPES = \{(.*?)^\}", src, re.S | re.M).group(1)


def shape_of(name):
    m = re.search(r'^\s{4}"' + re.escape(name) + r'":\s*\(', body, re.M)
    assert m, name
    i = m.end() - 1
    d, j = 0, i
    while j < len(body):
        if body[j] == "(":
            d += 1
        elif body[j] == ")":
            d -= 1
            if d == 0:
                break
        j += 1
    return ast.literal_eval(body[i:j + 1])


def enc(parts):
    out = b"*%d\r\n" % len(parts)
    for p in parts:
        b = p.encode()
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


d = tempfile.mkdtemp(prefix="shapeverify_", dir="/data/tmp")
p = subprocess.Popen([f"{S}/bins/b6b_after1.elf", "--port", "7471", "--dir", d, "--save", ""],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    for _ in range(200):
        try:
            s = socket.create_connection(("127.0.0.1", 7471), timeout=0.5)
            break
        except OSError:
            time.sleep(0.05)
    s.settimeout(3)
    bad = 0
    for name in NEW:
        seed, cmd = shape_of(name)
        for sc in seed:
            s.sendall(enc(sc.split()))
            s.recv(65536)
        s.sendall(enc(cmd))
        time.sleep(0.05)
        rep = s.recv(65536)
        head = rep.split(b"\r\n")[0]
        flag = ""
        if rep.startswith(b"-"):
            flag = "   <<< ERROR REPLY"
            bad += 1
        elif rep.startswith((b"$-1", b"*-1", b"_")):
            flag = "   <<< NIL"
            bad += 1
        print(f"{name:16} {' '.join(cmd):48} -> {head!r}{flag}")
    print(f"\n{len(NEW)} shapes, {bad} returning error/nil")
    sys.exit(1 if bad else 0)
finally:
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
