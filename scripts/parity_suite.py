#!/usr/bin/env python3
"""frankenredis parity / migration-safety suite runner.

A single entry point that runs the modern, self-contained parity gates against a
vendored redis 7.2.4 oracle and prints a PASS/FAIL scorecard (exit non-zero on
any failure). Intended as a release-readiness / migration-safety check — proves
fr is behavior-byte-exact and bidirectionally interop-compatible with redis.

Two gate classes are run:
  * self-orchestrating cross-compat gates (spin up their own server quartets):
      rdb_cross_compat_gate, aof_cross_compat_gate, replication_cross_compat_gate
  * port-based parity gates (run against one shared fr+redis pair):
      dump_byte_equality_gate, introspection_semantics_gate,
      rare_write_state_gate, keyspace_accounting_gate,
      cmdstat_keyspace_parity_gate, command_getkeys_gate,
      flag_error_edge_gate (flag-conflict / error-order / encoding boundaries)

Usage: parity_suite.py <redis-server-bin> <fr-bin>
Both servers are launched with --enable-debug-command; the redis oracle is
started from a clean cwd so it never loads stale dump.rdb / appendonly files.
"""
import sys, os, time, socket, subprocess

REDIS_BIN = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "legacy_redis_code/redis/src/redis-server")
FR_BIN = os.path.abspath(sys.argv[2] if len(sys.argv) > 2 else "/tmp/fr_rdb")
HERE = os.path.dirname(os.path.abspath(__file__))
ORACLE_PORT, FR_PORT = 29951, 29952


def enc(a):
    o = b"*%d\r\n" % len(a)
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        o += b"$%d\r\n%s\r\n" % (len(x), x)
    return o


def ping(port):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=1)
        s.sendall(enc(["PING"]))
        time.sleep(0.03)
        ok = b"PONG" in s.recv(64)
        s.close()
        return ok
    except Exception:
        return False


def wait_up(port, deadline=8):
    t0 = time.time()
    while time.time() - t0 < deadline:
        if ping(port):
            return True
        time.sleep(0.1)
    return False


def run_gate(name, argv):
    try:
        r = subprocess.run([sys.executable, os.path.join(HERE, name)] + argv,
                           capture_output=True, text=True, timeout=180)
        ok = r.returncode == 0
        tail = (r.stdout.strip().splitlines() or ["(no output)"])[-1]
        return ok, tail
    except subprocess.TimeoutExpired:
        return False, "TIMEOUT"
    except Exception as e:
        return False, f"ERROR {e}"


SELF_ORCH = [
    ("rdb_cross_compat_gate.py", [REDIS_BIN, FR_BIN, "29961"]),
    ("aof_cross_compat_gate.py", [REDIS_BIN, FR_BIN, "29971"]),
    ("replication_cross_compat_gate.py", [REDIS_BIN, FR_BIN, "29981"]),
]
# Gates invoked positionally: <oracle_port> <fr_port>
PORT_BASED = [
    ("dump_byte_equality_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("introspection_semantics_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("rare_write_state_gate.py", ["7", "1500"]),  # uses ORACLE_PORT/FR_PORT env
    ("keyspace_accounting_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("cmdstat_keyspace_parity_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("command_getkeys_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("ttl_semantics_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("move_swapdb_expiry_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("multi_db_relocation_fuzz.py", [str(ORACLE_PORT), str(FR_PORT), "2", "800"]),
    ("cross_db_type_relocation_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("tracking_invalidation_lifecycle_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("watch_semantics_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("multi_exec_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("validation_order_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # self-heals encoding thresholds on both servers before comparing, so it is
    # immune to a stray CONFIG SET left by an earlier gate on the shared oracle.
    ("flag_error_edge_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # non-zero-DB error/key parity + internal-namespace (\0frdb\0) leak guard.
    ("multidb_namespace_leak_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # feature-completeness: every redis 7.2.4 command + container subcommand is wired up.
    ("command_coverage_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # zset total-order under heavy equal-score ties + binary members — guards the
    # FullSortedSet member-storage/index rewrites (peni2 Arc sharing, uybhq follow-up).
    ("zset_tiebreak_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # grouped stream index (rax-of-listpacks, p8wd1 74a926418): node-boundary
    # reads + RDB round-trip.
    ("stream_node_grouping_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # sealed quicklist chunks (Owned->Listpack, 99fwc 8c2421045): sealed reads +
    # LSET/LINSERT/LREM re-materialization + DUMP->RESTORE cross-impl.
    ("list_chunk_seal_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # OBJECT ENCODING + content survive an RDB round-trip across every type's
    # encoding boundary — guards the RAM-compaction save/load paths (61e3p class).
    ("encoding_reload_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # RESP3 (HELLO 3) reply-TYPE markers (maps/sets/doubles/nulls) across the
    # collection + introspection commands that change shape under RESP3.
    ("resp3_reply_type_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
]

# Older differs use argparse flags: --oracle <port> --fr <port>
ARGPARSE_BASED = [
    "float_format_differ.py", "zset_differ.py", "hash_differ.py", "set_differ.py",
    "list_differ.py", "geo_differ.py", "arity_error_differ.py", "bitmap_differ.py",
    "sort_differ.py", "scan_differ.py",
]


def main():
    results = []

    # --- self-orchestrating cross-compat gates ---
    for name, argv in SELF_ORCH:
        if not os.path.exists(os.path.join(HERE, name)):
            continue
        ok, tail = run_gate(name, argv)
        results.append((name, ok, tail))

    # --- shared fr+redis pair for port-based gates ---
    procs = []
    os.environ["ORACLE_PORT"] = str(ORACLE_PORT)
    os.environ["FR_PORT"] = str(FR_PORT)
    try:
        procs.append(subprocess.Popen(
            [REDIS_BIN, "--port", str(ORACLE_PORT), "--save", "", "--appendonly", "no",
             "--enable-debug-command", "yes"],
            cwd="/tmp", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        procs.append(subprocess.Popen(
            [FR_BIN, "--port", str(FR_PORT), "--enable-debug-command", "yes"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        pair_ok = wait_up(ORACLE_PORT) and wait_up(FR_PORT)
        for name, argv in PORT_BASED:
            if not os.path.exists(os.path.join(HERE, name)):
                continue
            if not pair_ok:
                results.append((name, False, "fr/redis pair did not start"))
                continue
            ok, tail = run_gate(name, argv)
            results.append((name, ok, tail))
        for name in ARGPARSE_BASED:
            if not os.path.exists(os.path.join(HERE, name)):
                continue
            if not pair_ok:
                results.append((name, False, "fr/redis pair did not start"))
                continue
            ok, tail = run_gate(name, ["--oracle", str(ORACLE_PORT), "--fr", str(FR_PORT)])
            results.append((name, ok, tail))
    finally:
        for p in procs:
            p.terminate()
        time.sleep(0.3)
        for p in procs:
            try:
                p.kill()
            except Exception:
                pass

    # --- scorecard ---
    print("\n" + "=" * 72)
    print("frankenredis parity / migration-safety suite")
    print("=" * 72)
    passed = sum(1 for _, ok, _ in results if ok)
    for name, ok, tail in results:
        mark = "PASS" if ok else "FAIL"
        print(f"  [{mark}] {name:<40} {tail[:80]}")
    print("-" * 72)
    print(f"  {passed}/{len(results)} gates passed")
    sys.exit(0 if passed == len(results) else 1)


main()
