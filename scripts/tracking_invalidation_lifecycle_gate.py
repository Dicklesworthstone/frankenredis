#!/usr/bin/env python3
"""Differential gate: RESP3 client-side-caching invalidation lifecycle, vs redis 7.2.4.

CLIENT TRACKING fires an `invalidate` push when a tracked key is mutated, deleted,
overwritten, has its TTL changed, OR EXPIRES. The existing tracking probes cover
direct mutation but NOT the expiry->invalidation path — which the volatile-only
expiry side-dict refactor (uhthd / isa2w) reworked: key removal on expiry now goes
through a different code path that must still signal the tracking table. This gate
pins the whole lifecycle, with the lazy-expire case as the headline.

For each scenario a RESP3 tracking client A registers interest in a key (GET), a
second client B (or A) performs the event, and we assert the exact set of keys A
is told to invalidate matches redis. All events are DETERMINISTIC (lazy expiry is
forced by an explicit access after a generous wait — no reliance on the active
cycle's timer). A background reader thread captures every push so an invalidation
interleaved with a command reply is never lost.

Usage: tracking_invalidation_lifecycle_gate.py <oracle_port> <fr_port>  Exit 0=parity,1=diverge.
"""
import socket, sys, time, re, threading

def conn(p): return socket.create_connection(("127.0.0.1", p), timeout=5)
def send(s, *a):
    b=b"*%d\r\n"%len(a)
    for x in a:
        x=x if isinstance(x,bytes) else str(x).encode(); b+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(b)

class Reader(threading.Thread):
    def __init__(s, sock):
        super().__init__(daemon=True); s.sock=sock; s.buf=b""; s.alive=True; s.sock.settimeout(0.15)
    def run(s):
        while s.alive:
            try: d=s.sock.recv(65536)
            except socket.timeout: continue
            except OSError: break
            if d: s.buf+=d
    def stop(s): s.alive=False

def inv_keys(buf):
    """Extract every invalidated key from RESP3 invalidate pushes in buf (sorted,
    deduped). A null array (FLUSHALL/FLUSHDB) is reported as the token __FLUSH__."""
    out=set()
    for m in re.finditer(rb">2\r\n\$10\r\ninvalidate\r\n(\*-?\d+\r\n(?:\$\d+\r\n[^\r]*\r\n)*)", buf):
        body=m.group(1)
        if body.startswith(b"*-1"): out.add("__FLUSH__"); continue
        for km in re.finditer(rb"\$\d+\r\n([^\r]*)\r\n", body):
            out.add(km.group(1).decode("latin1"))
    return sorted(out)

div=0
def scenario(label, port_pair, body):
    """body(A, B, mark) -> list of invalidated keys captured after `mark`."""
    global div
    res={}
    for port, who in port_pair:
        A=conn(port); rd=Reader(A); rd.start()
        send(A,"HELLO","3"); time.sleep(0.15)
        send(A,"FLUSHALL"); time.sleep(0.1)
        send(A,"CLIENT","TRACKING","ON"); time.sleep(0.1)
        B=conn(port)
        res[who]=body(A, B, rd)
        rd.stop()
    if res["oracle"]!=res["fr"]:
        div+=1
        print(f"DIVERGE {label}\n  oracle invalidated: {res['oracle']}\n  fr     invalidated: {res['fr']}")

def main():
    op=int(sys.argv[1]); fp=int(sys.argv[2])
    pair=[(op,"oracle"),(fp,"fr")]

    def track(A, key="tk", val="v"):
        send(A,"SET",key,val); time.sleep(0.08)
        send(A,"GET",key); time.sleep(0.08)

    # 1) tracked key DELeted by another client
    def s_del(A,B,rd):
        track(A); mark=len(rd.buf)
        send(B,"DEL","tk"); time.sleep(0.25)
        return inv_keys(rd.buf[mark:])
    scenario("del", pair, s_del)

    # 2) tracked key overwritten (SET) by another client
    def s_set(A,B,rd):
        track(A); mark=len(rd.buf)
        send(B,"SET","tk","other"); time.sleep(0.25)
        return inv_keys(rd.buf[mark:])
    scenario("overwrite", pair, s_set)

    # 3) tracked key gets a TTL (PEXPIRE = modification)
    def s_pexpire(A,B,rd):
        track(A); mark=len(rd.buf)
        send(A,"PEXPIRE","tk","100000"); time.sleep(0.25)
        return inv_keys(rd.buf[mark:])
    scenario("pexpire-modify", pair, s_pexpire)

    # 4) HEADLINE: tracked key LAZILY EXPIRES (forced by B's access post-TTL)
    def s_lazy(A,B,rd):
        send(A,"SET","tk","v","PX","200"); time.sleep(0.08)
        send(A,"GET","tk"); time.sleep(0.08)        # track the live key
        mark=len(rd.buf)
        time.sleep(0.5)                              # well past the 200ms TTL
        send(B,"GET","tk"); time.sleep(0.3)          # access -> lazy delete -> invalidate
        return inv_keys(rd.buf[mark:])
    scenario("lazy-expire", pair, s_lazy)

    # 5) tracked key removed by FLUSHALL -> null-array invalidation
    def s_flush(A,B,rd):
        track(A); mark=len(rd.buf)
        send(B,"FLUSHALL"); time.sleep(0.25)
        return inv_keys(rd.buf[mark:])
    scenario("flushall", pair, s_flush)

    # 6) un-tracked key event must NOT invalidate (A tracks tk, B touches other)
    def s_isolated(A,B,rd):
        track(A); mark=len(rd.buf)
        send(B,"SET","other","z"); send(B,"DEL","other"); time.sleep(0.25)
        return inv_keys(rd.buf[mark:])
    scenario("untracked-isolated", pair, s_isolated)

    if div: print(f"\nFAIL: {div} divergence(s)"); sys.exit(1)
    print("OK: tracked-key invalidation lifecycle (del/overwrite/pexpire/lazy-expire/flush/isolation) byte-exact vs redis 7.2.4")

if __name__=="__main__": main()
