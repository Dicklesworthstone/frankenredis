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


def _free_port(preferred):
    """Return `preferred` if no server is already listening on it, else the next
    free port above it. Without this, a STALE server left on a fixed port by a
    prior/killed run silently captures the suite: our own Popen'd server fails to
    bind (EADDRINUSE) and exits, but `wait_up()` still PINGs the stale process,
    so every port-based gate runs against the WRONG server and reports a cascade
    of false FAILs (observed: 9/45 with the real binary, 45/45 after clearing the
    stale server). Probe by CONNECTing (not binding — bind is unavailable under
    the sandbox): a refused connection means the port is free for us to claim."""
    for port in range(preferred, preferred + 400):
        try:
            c = socket.create_connection(("127.0.0.1", port), timeout=0.2)
            c.close()  # something is listening here -> occupied, try the next
        except OSError:
            return port  # connection refused / unreachable -> free
    return preferred


ORACLE_PORT, FR_PORT = _free_port(29951), _free_port(29952)
# Self-orchestrating gates start their own server quartets; give each a base port
# we've verified is free for the same stale-server reason.
SO_PORTS = {name: _free_port(base) for name, base in (
    ("rdb", 29961), ("aof", 29971), ("repl", 29981),
    ("replf", 29991), ("aoff", 29671), ("rdbc", 29761))}


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
    ("rdb_cross_compat_gate.py", [REDIS_BIN, FR_BIN, str(SO_PORTS["rdb"])]),
    ("aof_cross_compat_gate.py", [REDIS_BIN, FR_BIN, str(SO_PORTS["aof"])]),
    ("replication_cross_compat_gate.py", [REDIS_BIN, FR_BIN, str(SO_PORTS["repl"])]),
    ("replication_digest_fuzz.py", [REDIS_BIN, FR_BIN, str(SO_PORTS["replf"]), "3", "500"]),
    ("aof_roundtrip_digest_fuzz.py", [REDIS_BIN, FR_BIN, str(SO_PORTS["aoff"]), "2", "4"]),
    ("rdb_changes_save_gate.py", [REDIS_BIN, FR_BIN, str(SO_PORTS["rdbc"])]),
]
# Gates invoked positionally: <oracle_port> <fr_port>
PORT_BASED = [
    ("dump_byte_equality_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("introspection_semantics_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("debug_multidb_key_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("memory_usage_multidb_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("info_memory_flush_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("keyspace_expires_count_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("reload_digest_fidelity_gate.py", [str(ORACLE_PORT), str(FR_PORT), "3", "20"]),
    ("rare_write_state_gate.py", ["7", "1500"]),  # uses ORACLE_PORT/FR_PORT env
    ("keyspace_accounting_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("cmdstat_keyspace_parity_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("command_getkeys_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("ttl_semantics_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("move_swapdb_expiry_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("multi_db_relocation_fuzz.py", [str(ORACLE_PORT), str(FR_PORT), "2", "800"]),
    ("cross_db_type_relocation_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("copy_command_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("lpos_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("multikey_pop_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("tracking_invalidation_lifecycle_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("client_tracking_differential_probe.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("digest_state_fuzz.py", [str(ORACLE_PORT), str(FR_PORT), "4", "1200"]),
    ("watch_semantics_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("multi_exec_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("reset_state_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("validation_order_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("bitfield_differ.py", [str(ORACLE_PORT), str(FR_PORT), "1", "1200"]),
    # self-heals encoding thresholds on both servers before comparing, so it is
    # immune to a stray CONFIG SET left by an earlier gate on the shared oracle.
    ("flag_error_edge_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # non-zero-DB error/key parity + internal-namespace (\0frdb\0) leak guard.
    ("multidb_namespace_leak_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # feature-completeness: every redis 7.2.4 command + container subcommand is wired up.
    ("command_coverage_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("acl_cat_membership_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # borrowed byte-prefix fast-path packets (EXISTS/MGET/MSET/INCR/.../LPUSH/LPOP/
    # ZPOPMIN/...) byte-exact under RESP2+RESP3 — guards the whole fast-path surface
    # so a dispatch-chain regression is caught in the suite.
    ("packet_fastpath_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # fast-path WRITES must emit the same keyspace events as the generic path.
    ("fastpath_keyspace_events_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # zset total-order under heavy equal-score ties + binary members — guards the
    # FullSortedSet member-storage/index rewrites (peni2 Arc sharing, uybhq follow-up).
    ("zset_tiebreak_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # grouped stream index (rax-of-listpacks, p8wd1 74a926418): node-boundary
    # reads + RDB round-trip.
    ("stream_node_grouping_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # adversarial stream DUMP/RESTORE/RELOAD byte-exactness across node
    # boundaries + schemas + XDEL/XTRIM/XSETID + groups/PEL — guards the
    # bulk-build (879cce121/a3b513b40) + DUMP encode (1aae17b9f/3fb6584f3) paths
    # and the tombstone round-trip invariant (vbacn).
    ("stream_dump_reload_fuzz.py", [str(ORACLE_PORT), str(FR_PORT), "40"]),
    # sealed quicklist chunks (Owned->Listpack, 99fwc 8c2421045): sealed reads +
    # LSET/LINSERT/LREM re-materialization + DUMP->RESTORE cross-impl.
    ("list_chunk_seal_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # OBJECT ENCODING + content survive an RDB round-trip across every type's
    # encoding boundary — guards the RAM-compaction save/load paths (61e3p class).
    ("encoding_reload_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("object_policy_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("lfu_idletime_policy_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("lfu_idletime_write_reaccess_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    ("restore_idletime_freq_differ.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # RESP3 (HELLO 3) reply-TYPE markers (maps/sets/doubles/nulls) across the
    # collection + introspection commands that change shape under RESP3.
    ("resp3_reply_type_gate.py", [str(ORACLE_PORT), str(FR_PORT)]),
    # adversarial random fuzz of the UNDER-tested ("untrodden") command surface —
    # the commands the type-specific differs don't exhaustively cover. Guards the
    # least-exercised paths against silent regression (60k-iter clean as of now).
    ("fuzz_untrodden_differ.py", [str(ORACLE_PORT), str(FR_PORT), "--seed", "9001", "--iters", "1200"]),
]

# Older differs use argparse flags: --oracle <port> --fr <port>
ARGPARSE_BASED = [
    "client_kill_differ.py",
    "float_format_differ.py", "zset_differ.py", "hash_differ.py", "set_differ.py",
    "list_differ.py", "geo_differ.py", "arity_error_differ.py", "bitmap_differ.py",
    "sort_differ.py", "scan_differ.py", "hexfloat_incr_differ.py",
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
        # Backstop health check: confirm the oracle we reached is a real redis
        # 7.2.4 (a wedged/stale/wrong server answers PING but reports a tiny
        # COMMAND COUNT). Catches any wedge cause the free-port guard misses, so
        # the suite aborts loudly instead of mislabeling 30 gates as FAIL.
        if pair_ok:
            try:
                c = socket.create_connection(("127.0.0.1", ORACLE_PORT), timeout=2)
                c.sendall(enc(["COMMAND", "COUNT"]))
                time.sleep(0.05)
                reply = c.recv(64)
                c.close()
                n = int(reply[1:reply.index(b"\r\n")]) if reply[:1] == b":" else 0
                if n < 200:
                    pair_ok = False
                    print(f"  [ABORT] oracle on :{ORACLE_PORT} reported COMMAND COUNT={n} "
                          f"(<200) — not a healthy redis 7.2.4; skipping port-based gates")
            except Exception as e:
                pair_ok = False
                print(f"  [ABORT] oracle health check on :{ORACLE_PORT} failed: {e}")
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
