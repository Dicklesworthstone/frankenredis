#!/usr/bin/env python3
"""Which commands does the borrowed-dispatch FLOOR TABLE name, and which does it miss?

A command absent from the table cannot be front-classified: the classifier runs, declines,
and the frame walks the cascade -- which is what GET was measured doing at 126.0 instr/op
(31 pct of its dispatch).

DETECTOR HAZARDS, both of which have produced wrong answers on this table before:
  1. The arms match BYTE ARRAYS -- `[b'T', b'O', b'U', b'C', b'H']`, not `b"TOUCH"`. A
     string-literal grep reports every classified command as stranded.
  2. Long names WRAP across source lines, so a line-oriented regex silently misses them.
Both are guarded by CANARIES below: TOUCH (short, classified) and ZINTERCARD (long enough to
wrap, classified) must both be found, or the census refuses to print.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MAIN = os.path.join(ROOT, "crates", "fr-server", "src", "main.rs")
CMD_DIR = os.path.join(ROOT, "legacy_redis_code", "redis", "src", "commands")

src = open(MAIN, encoding="utf-8", errors="replace").read()

# Byte-array arms, allowing arbitrary whitespace/newlines between elements.
ARM = re.compile(r"\[\s*((?:b'[A-Za-z0-9_.-]'\s*,\s*)+b'[A-Za-z0-9_.-]')\s*\]", re.S)
tokens = set()
for m in ARM.finditer(src):
    letters = re.findall(r"b'([A-Za-z0-9_.-])'", m.group(1))
    if letters:
        tokens.add("".join(letters).upper())

for canary in ("TOUCH", "ZINTERCARD"):
    if canary not in tokens:
        print("DETECTOR FAILED: canary %s not found -- the regex is wrong, refusing to "
              "print a census that would call classified commands stranded." % canary)
        sys.exit(2)

# The full command surface, from the vendored Redis command JSON (container commands like
# `CONFIG|GET` live in files named config-get.json; take the top-level name only).
surface = set()
for name in os.listdir(CMD_DIR):
    if not name.endswith(".json"):
        continue
    stem = name[:-5]
    if "-" in stem:          # subcommand file: parent is what dispatch sees first
        stem = stem.split("-")[0]
    surface.add(stem.upper())

print("floor table names %d tokens; command surface has %d top-level commands"
      % (len(tokens), len(surface)))
print("canaries OK (TOUCH, ZINTERCARD both present)\n")

# Commands most likely to matter: the ones redis-benchmark and this campaign actually drive.
HOT = ["GET", "SET", "INCR", "DECR", "SETNX", "GETSET", "APPEND", "STRLEN", "EXISTS", "TTL",
       "EXPIRE", "DEL", "TYPE", "LPUSH", "RPUSH", "LPOP", "RPOP", "LRANGE", "LLEN",
       "SADD", "SREM", "SMEMBERS", "SISMEMBER", "SCARD", "HSET", "HGET", "HDEL", "HGETALL",
       "ZADD", "ZREM", "ZSCORE", "ZCARD", "ZRANGE", "MSET", "MGET", "PING", "ECHO"]
missing_hot = [c for c in HOT if c not in tokens]
print("HOT COMMANDS NOT IN THE FLOOR TABLE (%d of %d):" % (len(missing_hot), len(HOT)))
for c in missing_hot:
    print("   %-12s %s" % (c, "<- in command surface" if c in surface else "(not a surface cmd)"))
print("\nHOT COMMANDS THAT ARE CLASSIFIED (%d):" % (len(HOT) - len(missing_hot)))
print("   " + " ".join(c for c in HOT if c in tokens))
