#!/usr/bin/env python3
"""Differential gate: an RDB round-trip (DEBUG RELOAD) preserves the whole-keyspace
DEBUG DIGEST, and fr stays converged with redis 7.2.4 across it.

encoding_reload_gate checks OBJECT ENCODING + DIGEST-VALUE survive a reload for a
fixed set of boundary keys. This gate is broader: it drives a deterministic random
multi-DB command stream (reusing digest_state_fuzz's generator), then every round
issues DEBUG RELOAD to BOTH servers and asserts (a) fr's whole-DB digest is
UNCHANGED by the reload — i.e. the RDB save+load round-trips every value/encoding/
TTL/DB-placement faithfully — and (b) fr's post-reload digest still equals redis's.
A regression in any fr-store RDB encode/decode path surfaces as a digest that
shifts across the reload or diverges from redis.

Usage: reload_digest_fidelity_gate.py <oracle_port> <fr_port> [seeds] [rounds]
       default 4 seeds x 25 rounds (~40 cmds/round).  Exit 0=parity,1=divergence.
"""
import importlib.util, os, random, sys

# Reuse the committed deterministic command generator + RESP client.
_dsf_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "digest_state_fuzz.py")
_spec = importlib.util.spec_from_file_location("digest_state_fuzz", _dsf_path)
M = importlib.util.module_from_spec(_spec); _spec.loader.exec_module(M)

def main():
    od = M.R(int(sys.argv[1])); fr = M.R(int(sys.argv[2]))
    seeds = int(sys.argv[3]) if len(sys.argv) > 3 else 4
    rounds = int(sys.argv[4]) if len(sys.argv) > 4 else 25
    per = 40
    for sd in range(seeds):
        rnd = random.Random(5500 + sd)
        for s in (od, fr):
            for db in range(3): s.cmd("select", str(db)); s.cmd("flushall")
            s.cmd("select", "0")
        recent = []
        for r in range(rounds):
            for _ in range(per):
                cmd = M.gen(rnd)
                ro = od.cmd(*cmd); rf = fr.cmd(*cmd)
                recent.append((cmd, ro, rf)); recent = recent[-30:]
                if ro != rf:
                    print(f"[seed {5500+sd}] REPLY DIVERGE {cmd}\n  oracle={ro!r}\n  fr={rf!r}")
                    sys.exit(1)
            d_before = fr.cmd("debug", "digest")
            od.cmd("debug", "reload"); fr.cmd("debug", "reload")
            d_after = fr.cmd("debug", "digest"); o_after = od.cmd("debug", "digest")
            if d_after != d_before:
                print(f"[seed {5500+sd} round {r}] fr DEBUG DIGEST changed across DEBUG RELOAD:")
                print(f"  before={d_before}\n  after ={d_after}")
                for c, a, b in recent[-15:]: print(f"    {c} -> O:{a!r} F:{b!r}")
                sys.exit(1)
            if d_after != o_after:
                print(f"[seed {5500+sd} round {r}] fr != redis after reload: redis={o_after} fr={d_after}")
                sys.exit(1)
    print(f"OK: {seeds} seed(s) x {rounds} reload-rounds — RDB round-trip digest fidelity + fr==redis convergence vs redis 7.2.4")

if __name__ == "__main__":
    main()
