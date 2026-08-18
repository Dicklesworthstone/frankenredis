#!/usr/bin/env python3
"""Pipelined stale-read-gate probe for the FastReply cache-clear removal.

The dispatch_route_differ is REQUEST-RESPONSE: one command per buffered pass, so the
read-gate cache is always fresh and the corpus cannot see this hazard at all. The cache
only persists WITHIN one buffered pass, so the only way a stale gate can be observed is to
pipeline a gate-INVALIDATING command and a borrowed read in a SINGLE write().

SELECT 1 makes the gate false (`selected_db != 0`). If the cached answer survived it, the
borrowed fast path would run against db 0 and return db 0's value where the correct answer
is db 1's. So the observable is a WRONG REPLY, not a crash.
"""
import socket, sys

def enc(*a):
    out = [b"*%d\r\n" % len(a)]
    for x in a:
        b = x.encode() if isinstance(x, str) else x
        out.append(b"$%d\r\n" % len(b) + b + b"\r\n")
    return b"".join(out)

def run(port, batch):
    s = socket.create_connection(("127.0.0.1", port), 5)
    s.sendall(b"".join(enc(*c) for c in batch))     # ONE write => one buffered pass
    buf = b""
    # (frankenredis-getexgate) DRAIN UNTIL QUIET rather than counting CRLFs. The old loop
    # stopped at `buf.count(CRLF) >= len(batch)`, which assumes one CRLF per reply -- false
    # for any bulk string, which carries two. That loop therefore stops EARLY on a batch
    # containing a bulk reply and silently drops the tail. Both arms are read the same way,
    # so the failure is not a crash: it is a divergence in the dropped tail that the probe
    # reports as MATCH. This probe exists to catch wrong replies, so a read that can hide
    # them defeats its only purpose. Draining to quiescence has no reply-shape assumption.
    s.settimeout(0.5)
    while True:
        try:
            chunk = s.recv(1 << 16)
        except socket.timeout:
            break
        if not chunk:
            break
        buf += chunk
    s.close()
    return buf

BATCHES = {
    # the load-bearing one: SELECT flips the gate mid-pass, then borrowed reads follow
    "select_then_reads": [
        ("SELECT", "0"), ("RPUSH", "gp:l", "a", "b", "c"), ("SET", "gp:s", "hello"),
        ("LLEN", "gp:l"), ("STRLEN", "gp:s"),
        ("SELECT", "1"),
        ("LLEN", "gp:l"), ("STRLEN", "gp:s"), ("SCARD", "gp:l"),
        ("SELECT", "0"),
        ("LLEN", "gp:l"), ("STRLEN", "gp:s"),
    ],
    # (frankenredis-getexgate) The four keymeta FastReply floor arms -- TTL, PTTL,
    # EXPIRETIME, PEXPIRETIME -- converted to the cached gate once bffba0601 stopped
    # clearing it. They share ONE executor and ONE predicate, so a stale gate would show up
    # on all four at once: inside the db 1 section every one of them must report the key
    # MISSING (-2), and a surviving db 0 cache would answer with db 0's real expiry instead.
    # The db 0 sections either side are what give that middle section its meaning -- without
    # them a gate stuck at "deny" would also print -2 and look identical to a pass.
    # NO EXPIRY IS DELIBERATE, AND THE FIRST VERSION OF THIS BATCH WAS WRONG. It set an
    # EXPIREAT and compared TTL and PTTL byte for byte, which DIVERGED on a correct build:
    # TTL and PTTL are CLOCK-RELATIVE, the two servers are separate processes queried at
    # different instants, and the replies differed by one second and ~500 ms. That is a
    # defect in the probe, not the engine -- a comparison that cannot hold on correct code
    # reports noise as a bug and, worse, trains the reader to ignore it.
    #
    # A key with no expiry removes the clock entirely while keeping the exact observable
    # this probe is for: every one of the four answers -1 (exists, no expiry) in db 0 and
    # -2 (missing) in db 1. A read gate that stayed cached across the SELECT would serve
    # db 0's -1 inside the db 1 section, which the byte compare catches. The db 0 sections
    # either side are what give the middle one meaning: a gate stuck at "deny" would send
    # everything down the generic path and still print -2 there, so without them a pass
    # would prove nothing.
    "select_then_keymeta": [
        ("SELECT", "0"), ("SET", "gp:k", "v"),
        ("TTL", "gp:k"), ("PTTL", "gp:k"), ("EXPIRETIME", "gp:k"), ("PEXPIRETIME", "gp:k"),
        ("SELECT", "1"),
        ("TTL", "gp:k"), ("PTTL", "gp:k"), ("EXPIRETIME", "gp:k"), ("PEXPIRETIME", "gp:k"),
        ("SELECT", "0"),
        ("TTL", "gp:k"), ("EXPIRETIME", "gp:k"),
    ],
    # The ABSOLUTE-time half, which a fixed EXPIREAT makes clock-free: EXPIRETIME and
    # PEXPIRETIME return the deadline itself rather than a remaining interval, so they are
    # comparable across two processes. This keeps an expiry-carrying key in the corpus --
    # the -1 batch above only exercises the no-expiry reply -- without reintroducing TTL or
    # PTTL, which cannot be byte-compared here at all.
    "select_then_keymeta_absolute": [
        ("SELECT", "0"), ("SET", "gp:e", "v"), ("EXPIREAT", "gp:e", "4102444800"),
        ("EXPIRETIME", "gp:e"), ("PEXPIRETIME", "gp:e"),
        ("SELECT", "1"),
        ("EXPIRETIME", "gp:e"), ("PEXPIRETIME", "gp:e"),
        ("SELECT", "0"),
        ("EXPIRETIME", "gp:e"), ("PEXPIRETIME", "gp:e"),
    ],
    # a read-only batch, to show the probe is not simply always divergent
    "reads_only": [
        ("SELECT", "0"), ("RPUSH", "gp:m", "x", "y"),
        ("LLEN", "gp:m"), ("LLEN", "gp:m"), ("LLEN", "gp:m"),
    ],
}

fr, rd = int(sys.argv[1]), int(sys.argv[2])
bad = 0
for name, batch in BATCHES.items():
    a, b = run(fr, batch), run(rd, batch)
    ok = a == b
    bad += 0 if ok else 1
    print(f"  {name:20s} {'MATCH' if ok else 'DIVERGES'}")
    if not ok:
        print(f"      fr    {a!r}")
        print(f"      redis {b!r}")
print(f"{len(BATCHES)} pipelined batches, {bad} divergence(s)")
sys.exit(0 if bad == 0 else 1)
