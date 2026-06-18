#!/usr/bin/env python3
"""multi_conn_differential_probe.py — live byte-exact parity check of fr-server
vs the vendored redis 7.2.4 oracle for the MULTI-CONNECTION STATEFUL surface
that the single-connection scripts/differential_probe.sh cannot reach:

  * pub/sub  — regular (SUBSCRIBE/PUBLISH/message), pattern (PSUBSCRIBE/pmessage),
               sharded (SSUBSCRIBE/SPUBLISH/smessage), and the PUBSUB
               CHANNELS/NUMSUB/NUMPAT/SHARDCHANNELS/SHARDNUMSUB introspection.
  * transactions — MULTI/EXEC/WATCH/UNWATCH/DISCARD/RESET, including the
               cross-connection WATCH dirty edges (concurrent write, expiry,
               FLUSHALL, same-value SET, nonexistent-key creation) that decide
               whether EXEC commits or aborts (nil).
  * client introspection edge cases that depend on live session state.
  * subscribe-mode command gating (RESP2) and RESP3 push-frame framing.

This is the harness pattern that found frankenredis-8ypwc (non-BCAST tracking
per-key invalidation). Run it after any change to pub/sub, transactions, client
tracking, or the session/runtime dispatch path.

SETUP (identical to differential_probe.sh — start the oracle CONFIG-LESS so the
compiled defaults align, build fr locally, run fr with --mode strict):
    ORACLE=legacy_redis_code/redis/src
    $ORACLE/redis-server --port 16399 --daemonize yes --save '' --appendonly no
    cargo build -p fr-server            # binary: $CARGO_TARGET_DIR/debug/frankenredis
    $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &
    python3 scripts/multi_conn_differential_probe.py 16399 16400

KNOWN non-bug divergence (normalized out below, do NOT report as a failure):
  * UNSUBSCRIBE / PUNSUBSCRIBE with no args (unsubscribe-all) emits the channels
    in the server's internal dict-iteration order, which differs between fr's
    container and redis's dict. The RESP contract only fixes that each channel
    appears exactly once and the running counts decrement N-1..0 — the channel
    ORDER is unspecified. We compare the unsubscribe-all reply as
    (set-of-channels, sorted-counts), ignoring order.
"""
import socket
import sys
import time


class Conn:
    """Minimal raw-socket RESP2/RESP3 client that can read replies and
    asynchronously-delivered push frames."""

    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=3)
        self.buf = b""

    def _fill(self):
        self.s.settimeout(0.6)
        try:
            d = self.s.recv(65536)
            if not d:
                return False
            self.buf += d
            return True
        except socket.timeout:
            return False

    def _read_line(self):
        while b"\r\n" not in self.buf:
            if not self._fill():
                return None
        line, self.buf = self.buf.split(b"\r\n", 1)
        return line

    def read_frame(self):
        line = self._read_line()
        if line is None:
            return None
        t, rest = line[:1], line[1:]
        if t in (b"+", b"-", b":", b",", b"#", b"("):
            return (t.decode(), rest.decode())
        if t == b"_":
            return None
        if t in (b"$", b"="):
            n = int(rest)
            if n < 0:
                return None
            while len(self.buf) < n + 2:
                if not self._fill():
                    break
            data, self.buf = self.buf[:n], self.buf[n + 2:]
            return data.decode("latin1")
        if t in (b"*", b">", b"%", b"~"):
            n = int(rest)
            if n < 0:
                return None
            count = n * 2 if t == b"%" else n
            return (t.decode(), [self.read_frame() for _ in range(count)])
        return ("?", line.decode("latin1"))

    def cmd(self, *args):
        out = b"*%d\r\n" % len(args)
        for a in args:
            a = a.encode() if isinstance(a, str) else a
            out += b"$%d\r\n%s\r\n" % (len(a), a)
        self.s.sendall(out)

    def cmd_read(self, *args):
        self.cmd(*args)
        return self.read_frame()

    def drain_push(self, wait=0.3):
        time.sleep(wait)
        frames = []
        while True:
            f = self.read_frame()
            if f is None:
                break
            frames.append(f)
        return frames

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def N(f):
    """Normalize a frame tree into a comparable structure."""
    if isinstance(f, tuple):
        tag, val = f
        if isinstance(val, list):
            return (tag, [N(x) for x in val])
        return (tag, val)
    return f


def norm_unsub_all(frames):
    """unsubscribe-all order is unspecified; compare as (channels, sorted counts)."""
    chans, counts = [], []
    for f in frames:
        fn = N(f)
        if isinstance(fn, tuple) and isinstance(fn[1], list) and len(fn[1]) == 3:
            chans.append(fn[1][1])
            counts.append(fn[1][2])
    return (sorted(chans), sorted(counts))


def run(port):
    R = {}

    def fresh():
        c = Conn(port)
        c.cmd_read("FLUSHALL")
        c.close()

    # ---------------- client introspection ----------------
    fresh()
    c = Conn(port)
    R["client_list_id_zero"] = N(c.cmd_read("CLIENT", "LIST", "ID", "0"))
    R["client_list_id_negative"] = N(c.cmd_read("CLIENT", "LIST", "ID", "-1"))
    R["client_list_id_absent"] = N(c.cmd_read("CLIENT", "LIST", "ID", "999999"))
    R["client_list_id_nonnumeric"] = N(c.cmd_read("CLIENT", "LIST", "ID", "abc"))
    c.close()

    # ---------------- pub/sub: regular ----------------
    fresh()
    sub, pub = Conn(port), Conn(port)
    R["sub_confirm"] = N(sub.cmd_read("SUBSCRIBE", "ch1"))
    R["publish_count"] = N(pub.cmd_read("PUBLISH", "ch1", "hello"))
    R["sub_message"] = [N(x) for x in sub.drain_push()]
    R["pubsub_channels"] = N(pub.cmd_read("PUBSUB", "CHANNELS"))
    R["pubsub_numsub"] = N(pub.cmd_read("PUBSUB", "NUMSUB", "ch1", "chX"))
    sub.close(); pub.close()

    # ---------------- pub/sub: pattern ----------------
    fresh()
    sub, pub = Conn(port), Conn(port)
    R["psub_confirm"] = N(sub.cmd_read("PSUBSCRIBE", "news.*"))
    R["ppublish_count"] = N(pub.cmd_read("PUBLISH", "news.tech", "x"))
    R["psub_message"] = [N(x) for x in sub.drain_push()]
    R["pubsub_numpat"] = N(pub.cmd_read("PUBSUB", "NUMPAT"))
    sub.close(); pub.close()

    # ---------------- pub/sub: sharded ----------------
    fresh()
    sub, pub = Conn(port), Conn(port)
    R["ssub_confirm"] = N(sub.cmd_read("SSUBSCRIBE", "sch1"))
    R["spublish_count"] = N(pub.cmd_read("SPUBLISH", "sch1", "smsg"))
    R["ssub_message"] = [N(x) for x in sub.drain_push()]
    R["pubsub_shardchannels"] = N(pub.cmd_read("PUBSUB", "SHARDCHANNELS"))
    R["pubsub_shardnumsub"] = N(pub.cmd_read("PUBSUB", "SHARDNUMSUB", "sch1", "schX"))
    R["spublish_nosub"] = N(pub.cmd_read("SPUBLISH", "schEMPTY", "v"))
    R["sunsub_confirm"] = N(sub.cmd_read("SUNSUBSCRIBE", "sch1"))
    sub.close(); pub.close()

    # sharded/regular namespace isolation: SUBSCRIBE'd channel ignores SPUBLISH
    fresh()
    sub, pub = Conn(port), Conn(port)
    sub.cmd_read("SUBSCRIBE", "iso"); sub.drain_push(0.1)
    R["regsub_gets_spublish"] = N(pub.cmd_read("SPUBLISH", "iso", "v"))
    R["regsub_spublish_msg"] = [N(x) for x in sub.drain_push(0.25)]
    sub.close(); pub.close()

    # ---------------- transactions: WATCH dirty edges ----------------
    # concurrent modify -> abort (nil)
    fresh()
    c, o = Conn(port), Conn(port)
    c.cmd_read("SET", "wk", "1"); c.cmd_read("WATCH", "wk"); c.cmd_read("MULTI")
    R["queued"] = N(c.cmd_read("SET", "wk", "2"))
    o.cmd_read("SET", "wk", "99")
    R["exec_aborted"] = N(c.cmd_read("EXEC"))
    R["wk_after_abort"] = N(c.cmd_read("GET", "wk"))
    c.close(); o.close()

    # no concurrent modify -> commit
    fresh()
    c = Conn(port)
    c.cmd_read("SET", "wk2", "1"); c.cmd_read("WATCH", "wk2"); c.cmd_read("MULTI")
    c.cmd_read("SET", "wk2", "5"); c.cmd_read("INCR", "wk2")
    R["exec_runs"] = N(c.cmd_read("EXEC"))
    c.close()

    # watched key expires -> abort
    fresh()
    c, o = Conn(port), Conn(port)
    c.cmd_read("SET", "we", "1"); c.cmd_read("PEXPIRE", "we", "40"); c.cmd_read("WATCH", "we")
    time.sleep(0.12)
    o.cmd_read("GET", "we")
    c.cmd_read("MULTI"); c.cmd_read("SET", "x", "1")
    R["watch_expired"] = N(c.cmd_read("EXEC"))
    c.close(); o.close()

    # FLUSHALL dirties watched key -> abort
    fresh()
    c, o = Conn(port), Conn(port)
    c.cmd_read("SET", "wf", "1"); c.cmd_read("WATCH", "wf")
    o.cmd_read("FLUSHALL")
    c.cmd_read("MULTI"); c.cmd_read("SET", "x", "1")
    R["watch_flushall"] = N(c.cmd_read("EXEC"))
    c.close(); o.close()

    # same-value SET still dirties (touch-based) -> abort
    fresh()
    c, o = Conn(port), Conn(port)
    c.cmd_read("SET", "ws", "same"); c.cmd_read("WATCH", "ws")
    o.cmd_read("SET", "ws", "same")
    c.cmd_read("MULTI"); c.cmd_read("GET", "ws")
    R["watch_same_value"] = N(c.cmd_read("EXEC"))
    c.close(); o.close()

    # create previously-nonexistent watched key -> abort
    fresh()
    c, o = Conn(port), Conn(port)
    c.cmd_read("WATCH", "wn")
    o.cmd_read("SET", "wn", "1")
    c.cmd_read("MULTI"); c.cmd_read("GET", "wn")
    R["watch_create"] = N(c.cmd_read("EXEC"))
    c.close(); o.close()

    # pure read by other conn does NOT dirty -> commit
    fresh()
    c, o = Conn(port), Conn(port)
    c.cmd_read("SET", "wr", "1"); c.cmd_read("WATCH", "wr")
    o.cmd_read("GET", "wr")
    c.cmd_read("MULTI"); c.cmd_read("GET", "wr")
    R["watch_pure_read"] = N(c.cmd_read("EXEC"))
    c.close(); o.close()

    # DISCARD unwatches
    fresh()
    c, o = Conn(port), Conn(port)
    c.cmd_read("SET", "wd", "1"); c.cmd_read("WATCH", "wd")
    c.cmd_read("MULTI"); c.cmd_read("DISCARD")
    o.cmd_read("SET", "wd", "2")
    c.cmd_read("MULTI"); c.cmd_read("GET", "wd")
    R["discard_unwatches"] = N(c.cmd_read("EXEC"))
    c.close(); o.close()

    # UNWATCH
    fresh()
    c, o = Conn(port), Conn(port)
    c.cmd_read("SET", "uw", "1"); c.cmd_read("WATCH", "uw"); c.cmd_read("UNWATCH")
    o.cmd_read("SET", "uw", "2")
    c.cmd_read("MULTI"); c.cmd_read("SET", "uw", "3")
    R["exec_after_unwatch"] = N(c.cmd_read("EXEC"))
    c.close(); o.close()

    # ---------------- transactions: error/abort semantics ----------------
    # unknown command -> EXECABORT
    fresh()
    c = Conn(port)
    c.cmd_read("MULTI")
    R["queued_bad"] = N(c.cmd_read("NOTACOMMAND", "x"))
    R["queued_good"] = N(c.cmd_read("SET", "g", "1"))
    R["exec_abort_err"] = N(c.cmd_read("EXEC"))
    c.close()

    # runtime error inside EXEC -> partial results with error element
    fresh()
    c = Conn(port)
    c.cmd_read("SET", "str", "abc"); c.cmd_read("MULTI")
    c.cmd_read("INCR", "str"); c.cmd_read("SET", "ok", "1")
    R["exec_partial"] = N(c.cmd_read("EXEC"))
    c.close()

    # nested MULTI -> error, txn stays open
    fresh()
    c = Conn(port)
    c.cmd_read("MULTI")
    R["nested_multi"] = N(c.cmd_read("MULTI"))
    c.cmd_read("SET", "a", "1")
    R["exec_after_nested"] = N(c.cmd_read("EXEC"))
    c.close()

    # WATCH inside MULTI -> queued error
    fresh()
    c = Conn(port)
    c.cmd_read("MULTI")
    R["watch_in_multi"] = N(c.cmd_read("WATCH", "k"))
    c.cmd_read("DISCARD")
    c.close()

    # EXEC / DISCARD without MULTI
    fresh()
    c = Conn(port)
    R["exec_no_multi"] = N(c.cmd_read("EXEC"))
    R["discard_no_multi"] = N(c.cmd_read("DISCARD"))
    R["exec_empty"] = N((lambda: (c.cmd_read("MULTI"), c.cmd_read("EXEC"))[1])())
    c.close()

    # RESET clears MULTI + subscribe state
    fresh()
    c = Conn(port)
    c.cmd_read("MULTI"); c.cmd_read("SET", "z", "1")
    R["reset_in_multi"] = N(c.cmd_read("RESET"))
    R["exec_after_reset"] = N(c.cmd_read("EXEC"))
    c.close()

    # ---------------- subscribe-mode gating + RESP3 framing ----------------
    # RESP2: disallowed command blocked, PING/SUBSCRIBE allowed
    fresh()
    c = Conn(port)
    c.cmd_read("SUBSCRIBE", "sc"); c.drain_push(0.1)
    R["sub_mode_get_blocked"] = N(c.cmd_read("GET", "k"))
    R["sub_mode_ping_ok"] = N(c.cmd_read("PING"))
    R["sub_mode_subscribe_ok"] = N(c.cmd_read("SUBSCRIBE", "sc2"))
    c.close()

    # RESP3: subscribe confirmation + message are PUSH frames; subscriber may
    # still run ordinary commands (frankenredis-j7nwu)
    fresh()
    sub, pub = Conn(port), Conn(port)
    sub.cmd_read("HELLO", "3"); sub.drain_push(0.1)
    R["resp3_sub_confirm"] = N(sub.cmd_read("SUBSCRIBE", "r3"))
    pub.cmd_read("PUBLISH", "r3", "hi")
    R["resp3_message"] = [N(x) for x in sub.drain_push()]
    R["resp3_sub_get"] = N(sub.cmd_read("GET", "missing"))
    sub.close(); pub.close()

    # multi-channel SUBSCRIBE confirmations + UNSUBSCRIBE-all (order normalized)
    fresh()
    c = Conn(port)
    c.cmd_read("SUBSCRIBE", "a", "b", "c")
    R["multi_subscribe"] = [N(x) for x in c.drain_push(0.2)]
    c.cmd("UNSUBSCRIBE")
    R["unsub_all"] = norm_unsub_all([c.read_frame() for _ in range(3)])
    c.close()

    # PUBSUB NUMSUB / SHARDNUMSUB with no args -> empty array
    fresh()
    c = Conn(port)
    R["numsub_empty"] = N(c.cmd_read("PUBSUB", "NUMSUB"))
    R["shardnumsub_empty"] = N(c.cmd_read("PUBSUB", "SHARDNUMSUB"))
    c.close()

    return R


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    o, f = run(op), run(fp)
    div = 0
    for k in o:
        ov, fv = o[k], f.get(k)
        if ov != fv:
            div += 1
            print(f"DIVERGE [{k}]\n     oracle: {ov}\n     fr    : {fv}")
    print("-" * 60)
    if div == 0:
        print(f"PASS — fr matches redis 7.2.4 across {len(o)} multi-connection scenarios")
    else:
        print(f"FAIL — {div} divergence(s)")
    sys.exit(1 if div else 0)


if __name__ == "__main__":
    main()
