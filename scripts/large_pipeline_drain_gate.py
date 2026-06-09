#!/usr/bin/env python3
"""large_pipeline_drain_gate.py — the event loop must fully drain reads that span
multiple recv() buffers, matching redis 7.2.4. Guards frankenredis-apg7r AND the
recv-drain optimization (7af658881).

The mio read path is edge-triggered: a single EPOLLIN must be drained, but only
while each recv() FILLS the 8 KiB buffer (more may be queued) — a partial read
means the socket is drained now (future data raises a fresh EPOLLIN). apg7r is the
correctness-critical case: a >8 KiB pipeline / bulk value delivered in one write
spans several recv() calls; if the loop stops early it STRANDS the tail bytes and
the connection HANGS. 7af658881 added the partial-read short-circuit (skip the
confirming EAGAIN recv) without reintroducing that stranding.

This gate sends, in a single socket write each: (1) a ~70 KB pipeline of 2000 SET
commands, (2) a 200 KB bulk value, (3) a 2000-deep pipeline mixing reads+writes —
and asserts every reply comes back (no hang) with the same results as redis. A
drain regression makes a reply count fall short within the timeout.

Self-launches a clean fr + redis pair. Usage: [--bin FR] [--redis-bin REDIS]
"""
import argparse, os, socket, subprocess, sys, tempfile, time


def send_all(s, data):
    s.sendall(data)


def read_until(s, needle_count, marker=b"\r\n", deadline_s=8.0):
    """Read until `marker` has occurred >= needle_count times, or timeout."""
    buf = b""
    end = time.time() + deadline_s
    s.settimeout(0.5)
    while time.time() < end:
        try:
            d = s.recv(1 << 16)
            if not d:
                break
            buf += d
            if buf.count(marker) >= needle_count:
                break
        except socket.timeout:
            continue
    return buf


def scenario(port):
    """Returns a dict of observable results, or 'HANG:<which>' on a stranded read."""
    s = socket.create_connection(("127.0.0.1", port), timeout=8)
    s.settimeout(8)
    out = {}

    # (1) ~70 KB pipeline: 2000 SET commands in ONE write (spans many recv bufs).
    pipe = bytearray()
    for i in range(2000):
        k = f"k{i}".encode(); v = f"v{i}".encode()
        pipe += b"*3\r\n$3\r\nSET\r\n$%d\r\n%s\r\n$%d\r\n%s\r\n" % (len(k), k, len(v), v)
    send_all(s, bytes(pipe))
    buf = read_until(s, 2000, b"+OK\r\n")
    ok = buf.count(b"+OK\r\n")
    out["pipeline_set_oks"] = ok
    if ok < 2000:
        out["hang"] = "pipeline_set"
        return out

    # (2) a key from the middle landed correctly.
    send_all(s, b"*2\r\n$3\r\nGET\r\n$5\r\nk1000\r\n")
    out["mid_key"] = read_until(s, 1, b"\r\n").split(b"\r\n")[1].decode("latin1") if True else None

    # (3) 200 KB bulk value in one write (spans many recv bufs).
    val = b"Z" * 200000
    send_all(s, b"*3\r\n$3\r\nSET\r\n$3\r\nbig\r\n$%d\r\n%s\r\n" % (len(val), val))
    out["big_set"] = read_until(s, 1, b"\r\n").startswith(b"+OK")
    send_all(s, b"*2\r\n$6\r\nSTRLEN\r\n$3\r\nbig\r\n")
    r = read_until(s, 1, b"\r\n")
    out["big_strlen"] = r.split(b"\r\n")[0].decode("latin1")

    # (4) 2000-deep mixed read/write pipeline in one write.
    pipe2 = bytearray()
    for i in range(2000):
        if i % 2 == 0:
            pipe2 += b"*2\r\n$4\r\nINCR\r\n$3\r\nctr\r\n"
        else:
            pipe2 += b"*2\r\n$3\r\nGET\r\n$5\r\nk%04d\r\n" % (i % 100)
    send_all(s, bytes(pipe2))
    buf2 = read_until(s, 2000, b"\r\n")
    out["mixed_replies"] = buf2.count(b"\r\n")
    if out["mixed_replies"] < 2000:
        out["hang"] = "mixed_pipeline"
        return out
    # final INCR value reflects 1000 increments.
    send_all(s, b"*2\r\n$3\r\nGET\r\n$3\r\nctr\r\n")
    out["ctr"] = read_until(s, 1, b"\r\n").split(b"\r\n")[1].decode("latin1")
    return out


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def wait_up(port, tries=60):
    for _ in range(tries):
        try:
            c = socket.create_connection(("127.0.0.1", port), 2)
            c.sendall(b"*1\r\n$4\r\nPING\r\n")
            if b"PONG" in c.recv(64):
                c.close(); return True
        except Exception:
            time.sleep(0.2)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.environ.get("FR_BIN",
                    "/data/tmp/cargo-target/release/frankenredis"))
    ap.add_argument("--redis-bin", default=os.environ.get("REDIS_BIN",
                    os.path.join(os.path.dirname(__file__), "..",
                                 "legacy_redis_code/redis/src/redis-server")))
    args = ap.parse_args()
    fr = os.path.abspath(args.bin); redis = os.path.abspath(args.redis_bin)
    if not os.path.exists(fr):
        print(f"SKIP: fr binary not found at {fr}"); return 0
    if not os.path.exists(redis):
        print(f"SKIP: redis-server not found at {redis}"); return 0

    rdir = tempfile.mkdtemp(prefix="fr_pipedrain_")
    fp, rp = free_port(), free_port()
    procs = [
        subprocess.Popen([fr, "--port", str(fp)],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
        subprocess.Popen([redis, "--port", str(rp), "--dir", rdir, "--save", "",
                          "--appendonly", "no"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL),
    ]
    try:
        if not (wait_up(fp) and wait_up(rp)):
            print("FAIL: servers did not start"); return 1
        f, r = scenario(fp), scenario(rp)
    finally:
        for p in procs: p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except Exception: p.kill()

    if "hang" in f:
        print(f"  [FAIL] fr STRANDED a multi-recv read ({f['hang']}) — drain regression "
              f"(apg7r / recvdrain). Got {f.get('pipeline_set_oks')} / mixed "
              f"{f.get('mixed_replies')}")
        print("FAIL — large-pipeline drain regression")
        return 1
    diffs = [(k, f.get(k), r.get(k)) for k in r if f.get(k) != r.get(k)]
    for k, a, b in diffs:
        print(f"  [DIFF] {k}: fr={a} rd={b}")
    if diffs:
        print("FAIL — large-pipeline results diverge from redis 7.2.4")
        return 1
    print("PASS — event loop fully drains multi-recv reads (2000-cmd pipeline, 200 KB "
          "value, mixed pipeline) matching redis 7.2.4 — apg7r / recvdrain guarded")
    return 0


if __name__ == "__main__":
    sys.exit(main())
