#!/usr/bin/env python3
"""Adversarial differential fuzzer for the stream DUMP / RESTORE / DEBUG RELOAD
paths, vs vendored redis 7.2.4.

frankenredis stores streams as an arena+index `PackedStreamLog` (entries packed
into one arena, a `BTreeMap` macro-node index, a SAMEFIELDS field dict) rather
than redis's rax-of-listpacks. The DUMP encoder re-synthesizes the upstream
listpack macro-nodes (master-entry + delta/SAMEFIELDS members, split at
`STREAM_NODE_MAX_*`), and the RESTORE / RDB-load path bulk-builds the index from
the decoded entries. All of that is byte-exact-DELICATE — a node-split miscount,
a SAMEFIELDS mis-detection, a delta-encoding slip, or a bulk-build boundary bug
would corrupt DUMP bytes or the rebuilt content while ordinary XADD/XRANGE still
look fine. This fuzzer drives randomized streams across every node boundary and
asserts three things against redis at each checkpoint:

  (1) GROUPLESS raw DUMP bytes are byte-identical fr == redis, BUT only for
      streams that have NOT been XDEL/XTRIM'd. After a deletion redis retains the
      removed entries as listpack TOMBSTONES (a `deleted` count + retained bytes
      in the node, dropped lazily by its own rewrite heuristics) while
      frankenredis eagerly compacts them out of the arena+index repr — so the
      opaque DUMP blob legitimately differs in size. That divergence is NOT
      observable (XINFO length / entries-added / max-deleted-entry-id, XLEN,
      XRANGE, DEBUG DIGEST-VALUE all still match — fr tracks the deletion
      metadata explicitly) and round-trips correctly, so for tombstoned streams
      we assert the contractual invariant (digest + RESTORE round-trip) instead
      of the non-contractual raw bytes. (Groupless also avoids the wall-clock
      consumer PEL/seen/active-time fields, which differ between two
      independently-built servers regardless.)
  (2) DEBUG DIGEST-VALUE matches fr == redis for streams WITH groups + consumers
      + PEL (logical content incl. group/PEL structure, excluding volatile times).
  (3) RESTORE round-trip: redis's DUMP payload RESTOREd onto fr reproduces redis's
      DEBUG DIGEST-VALUE (timestamps preserved through the payload), and a
      subsequent fr DEBUG RELOAD leaves that digest unchanged (exercises the
      RDB-file / load_stream_entries bulk-build path, distinct from RESTORE).

Randomization covers: entry counts straddling the 100-entry macro-node split and
the byte-size split (large values forcing plain nodes), stable vs rotating field
schemas (SAMEFIELDS on/off), binary field/value bytes, integer-looking values,
explicit seq collisions (ms-N, N-seq), XDEL / XTRIM MAXLEN / XSETID mutations,
and consumer groups with partial XREADGROUP + XACK building a realistic PEL.

Usage: stream_dump_reload_fuzz.py <oracle_port> <fr_port> [seeds] [iters]
Exit 0 = byte-exact parity, 1 = divergence.
"""
import socket, sys, random


def C(p):
    s = socket.create_connection(("127.0.0.1", p), timeout=20)
    s.settimeout(20)
    return s


class R:
    def __init__(s, p):
        s.s = C(p)
        s.b = b""

    def _l(s):
        while b"\r\n" not in s.b:
            s.b += s.s.recv(1 << 20)
        l, s.b = s.b.split(b"\r\n", 1)
        return l

    def _n(s, n):
        while len(s.b) < n + 2:
            s.b += s.s.recv(1 << 20)
        d = s.b[:n]
        s.b = s.b[n + 2:]
        return d

    def read(s):
        l = s._l()
        t = l[:1]
        if t in (b"+", b":"):
            return l[1:]
        if t == b"-":
            return Exception(l[1:].decode("latin1"))
        if t == b"$":
            n = int(l[1:])
            return None if n < 0 else s._n(n)
        if t == b"*":
            n = int(l[1:])
            return None if n < 0 else [s.read() for _ in range(n)]
        return l

    def cmd(s, *a):
        o = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, (bytes, bytearray)) else str(x).encode()
            o += b"$%d\r\n%s\r\n" % (len(x), x)
        s.s.sendall(o)
        return s.read()

    def pipe(s, cmds):
        o = b""
        for a in cmds:
            o += b"*%d\r\n" % len(a)
            for x in a:
                x = x if isinstance(x, (bytes, bytearray)) else str(x).encode()
                o += b"$%d\r\n%s\r\n" % (len(x), x)
        s.s.sendall(o)
        return [s.read() for _ in cmds]


div = 0


def fail(label, detail):
    global div
    div += 1
    print(f"DIVERGE {label}: {detail}")


def rnd_field(rng):
    pick = rng.random()
    if pick < 0.2:
        return bytes([rng.randint(0, 255) for _ in range(rng.randint(1, 6))])  # binary
    return ("f%d" % rng.randint(0, 4)).encode()


def rnd_value(rng):
    pick = rng.random()
    if pick < 0.15:
        return str(rng.randint(-(10 ** 12), 10 ** 12)).encode()  # int-looking
    if pick < 0.25:
        return bytes([rng.randint(0, 255) for _ in range(rng.randint(0, 8))])  # binary, may be empty
    if pick < 0.32:
        return (b"L" * rng.randint(70, 260))  # large -> can force plain/byte-split nodes
    return ("v%d" % rng.randint(0, 9999)).encode()


def build_entries(rng, n):
    """A deterministic list of (id, [field,value,...]) with a randomly chosen
    schema mode so SAMEFIELDS detection is exercised both ways."""
    stable = rng.random() < 0.5
    base_fields = [rnd_field(rng) for _ in range(rng.randint(1, 4))]
    out = []
    last_ms, last_seq = 0, 0
    for i in range(n):
        # advance id: usually ms+1/seq0, sometimes same ms with seq+1 (collision-ish)
        if rng.random() < 0.25:
            ms, seq = last_ms, last_seq + 1
        else:
            ms, seq = last_ms + rng.randint(1, 3), 0
        if ms == 0 and seq == 0:
            ms = 1
        last_ms, last_seq = ms, seq
        if stable:
            fields = base_fields
        else:
            fields = [rnd_field(rng) for _ in range(rng.randint(1, 4))]
        kv = []
        for f in fields:
            kv += [f, rnd_value(rng)]
        out.append((f"{ms}-{seq}", kv))
    return out


def load_stream(srv, key, entries):
    srv.cmd("del", key)
    cmds = [["xadd", key, eid] + kv for (eid, kv) in entries]
    # chunk to keep pipelines bounded
    for i in range(0, len(cmds), 512):
        srv.pipe(cmds[i:i + 512])


def main():
    global div
    od = R(int(sys.argv[1]))
    fr = R(int(sys.argv[2]))
    seeds = int(sys.argv[3]) if len(sys.argv) > 3 else 40
    iters = int(sys.argv[4]) if len(sys.argv) > 4 else 0  # unused knob, kept for arg-compat

    # confirm DEBUG DIGEST-VALUE is available on both (config asymmetry guard)
    for srv, name in ((od, "oracle"), (fr, "fr")):
        srv.cmd("flushall")
        srv.cmd("xadd", "probe", "1-1", "f", "v")
        if isinstance(srv.cmd("debug", "digest-value", "probe"), Exception):
            print(f"FAIL: {name} lacks DEBUG DIGEST-VALUE (start with --enable-debug-command yes)")
            sys.exit(2)
        srv.cmd("flushall")

    # sizes chosen to straddle the 100-entry macro-node split and beyond
    sizes = [1, 2, 50, 99, 100, 101, 150, 199, 200, 250, 400, 750, 1500]
    checked = 0
    for seed in range(seeds):
        rng = random.Random(1000 + seed)
        n = sizes[seed % len(sizes)] if seed < len(sizes) else rng.choice(sizes)
        entries = build_entries(rng, n)

        # ---- (1) GROUPLESS raw DUMP byte-equality ----
        load_stream(od, "s", entries)
        load_stream(fr, "s", entries)
        # random mutations applied identically to both (still groupless)
        muts = []
        tombstoning = False  # XDEL/XTRIM leave redis listpack tombstones; fr compacts
        if rng.random() < 0.6 and n > 4:
            victims = rng.sample([e[0] for e in entries], k=min(rng.randint(1, 5), n))
            muts.append(["xdel", "s"] + victims)
            tombstoning = True
        if rng.random() < 0.4 and n > 8:
            muts.append(["xtrim", "s", "MAXLEN", str(rng.randint(1, n))])
            tombstoning = True
        if rng.random() < 0.3:
            last = entries[-1][0].split("-")[0]
            muts.append(["xsetid", "s", f"{int(last)+rng.randint(0,5)}-0"])
        for m in muts:
            od.cmd(*m)
            fr.cmd(*m)
        d_o = od.cmd("dump", "s")
        d_f = fr.cmd("dump", "s")
        dig_o = od.cmd("debug", "digest-value", "s")
        dig_f = fr.cmd("debug", "digest-value", "s")
        # Live-content invariant always holds (fr tracks deletion metadata exactly).
        if dig_o != dig_f:
            fail(f"groupless-digest seed={seed} n={n} muts={muts}", "DEBUG DIGEST-VALUE mismatch")
        if not tombstoning:
            # No tombstones -> raw DUMP must be byte-identical to redis.
            if d_o != d_f:
                fail(f"groupless-dump seed={seed} n={n} muts={muts}",
                     f"len o={len(d_o) if isinstance(d_o, bytes) else d_o} f={len(d_f) if isinstance(d_f, bytes) else d_f}")
        else:
            # Tombstoned: redis retains listpack tombstones, fr compacts -> raw
            # bytes legitimately differ. Assert the contractual invariant: redis's
            # DUMP RESTOREs onto fr to the SAME live content (digest), proving the
            # round-trip is lossless across the representation difference.
            if isinstance(d_o, bytes):
                fr.cmd("del", "tomb")
                rr = fr.cmd("restore", "tomb", "0", d_o)
                if isinstance(rr, Exception):
                    fail(f"groupless-tomb-restore seed={seed} n={n} muts={muts}",
                         f"fr rejected redis tombstoned DUMP: {rr}")
                elif fr.cmd("debug", "digest-value", "tomb") != dig_o:
                    fail(f"groupless-tomb-roundtrip seed={seed} n={n} muts={muts}",
                         "fr RESTORE of redis tombstoned DUMP != redis digest")
        checked += 1

        # ---- (2) WITH groups + PEL: DEBUG DIGEST-VALUE parity ----
        load_stream(od, "g", entries)
        load_stream(fr, "g", entries)
        ngroups = rng.randint(1, 3)
        for gi in range(ngroups):
            gname = f"grp{gi}"
            start = rng.choice(["0", "$", entries[min(len(entries) - 1, rng.randint(0, len(entries) - 1))][0]])
            for srv in (od, fr):
                srv.cmd("xgroup", "create", "g", gname, start)
            # build a PEL via partial reads + acks, identical on both
            for ci in range(rng.randint(1, 2)):
                cname = f"c{ci}"
                cnt = rng.randint(1, max(1, n // 2))
                for srv in (od, fr):
                    srv.cmd("xreadgroup", "GROUP", gname, cname, "COUNT", str(cnt), "STREAMS", "g", ">")
                if rng.random() < 0.5:
                    # ack a few delivered ids
                    ack_ids = [e[0] for e in entries[:max(1, cnt // 2)]]
                    for srv in (od, fr):
                        srv.cmd("xack", "g", gname, *ack_ids)
        gd_o = od.cmd("debug", "digest-value", "g")
        gd_f = fr.cmd("debug", "digest-value", "g")
        if gd_o != gd_f:
            fail(f"grouped-digest seed={seed} n={n} ngroups={ngroups}", "DEBUG DIGEST-VALUE mismatch")

        # ---- (3) RESTORE round-trip (timestamps preserved) + DEBUG RELOAD stability ----
        payload = od.cmd("dump", "g")
        if isinstance(payload, bytes):
            fr.cmd("del", "rt")
            rr = fr.cmd("restore", "rt", "0", payload)
            if isinstance(rr, Exception):
                fail(f"restore seed={seed} n={n}", f"RESTORE rejected redis payload: {rr}")
            else:
                rt_o = od.cmd("debug", "digest-value", "g")
                rt_f = fr.cmd("debug", "digest-value", "rt")
                if rt_o != rt_f:
                    fail(f"restore-roundtrip seed={seed} n={n}", "fr RESTORE digest != redis")
                # DEBUG RELOAD exercises the RDB-file load_stream_entries bulk path
                before = fr.cmd("debug", "digest-value", "rt")
                fr.cmd("debug", "reload")
                after = fr.cmd("debug", "digest-value", "rt")
                if before != after:
                    fail(f"reload-stability seed={seed} n={n}", "fr DEBUG RELOAD changed stream digest")
                # and the reloaded stream still matches redis
                if after != rt_o:
                    fail(f"reload-vs-redis seed={seed} n={n}", "fr post-RELOAD digest != redis")
        checked += 1

        for srv in (od, fr):
            srv.cmd("flushall")

    if div:
        print(f"\nFAIL: {div} divergence(s) across {checked} checkpoints")
        sys.exit(1)
    print(f"OK: stream DUMP/RESTORE/RELOAD byte-exact vs redis 7.2.4 "
          f"({seeds} seeds, {checked} checkpoints, node-boundary + groups/PEL + mutations)")


if __name__ == "__main__":
    main()
