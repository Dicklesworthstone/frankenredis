#!/usr/bin/env bash
# rdb_cross_load_probe.sh — RDB persistence-interop differential gate.
#
# Verifies that fr-server can LOAD a dump.rdb produced by the vendored redis
# 7.2.4, reconstructing every value/type/encoding byte-for-byte. This is the
# reverse of the SAVE-compat check: fr already emits redis-loadable RDBs
# (project_rdb_stream_incompatibility); this gates the DECODE direction in
# crates/fr-persist (decode_rdb / rdb_stream.rs) against the real oracle.
#
# Method (apples-to-apples — both sides reload the SAME dump, so upstream's own
# post-reload semantics are the reference, not the live writer's in-memory state):
#   1. writer redis  -> populate a diverse corpus -> SAVE dump.rdb
#   2. fresh redis    reload dump.rdb           (reference: upstream reload)
#   3. fr-server      reload the same dump.rdb  (subject under test)
#   4. diff fr vs reloaded-redis across deterministic projections
#
# USAGE:
#   scripts/rdb_cross_load_probe.sh [FR_BIN]
#     FR_BIN defaults to $CARGO_TARGET_DIR/debug/frankenredis (build first:
#     CARGO_TARGET_DIR=/data/tmp/cargo-target cargo build -p fr-server).
#
# Previously-known divergences now FIXED and hard-gated below: FUNCTION library
# load/save round-trip (vd0ii/tm139/c0u9q) and stream consumer-group
# seen_time/active_time round-trip (sq4ov).
set -u

RS=${RS:-legacy_redis_code/redis/src/redis-server}
CLI=${CLI:-legacy_redis_code/redis/src/redis-cli}
FR_BIN=${1:-${CARGO_TARGET_DIR:-/data/tmp/cargo-target}/debug/frankenredis}
WP=${WP:-16599}   # writer redis
OP=${OP:-16600}   # reloaded reference redis
FP=${FP:-16601}   # fr-server under test

for b in "$RS" "$CLI" "$FR_BIN"; do
  [ -x "$b" ] || { echo "missing binary: $b"; exit 2; }
done

DIR=$(mktemp -d /tmp/fr-rdb-xload.XXXXXX)
W(){ $CLI -p "$WP" "$@" 2>&1; }
O(){ $CLI -p "$OP" "$@" 2>&1; }
F(){ $CLI -p "$FP" "$@" 2>&1; }

cleanup(){ $CLI -p "$WP" shutdown nosave >/dev/null 2>&1
           $CLI -p "$OP" shutdown nosave >/dev/null 2>&1
           kill "${FR_PID:-0}" >/dev/null 2>&1; }
trap cleanup EXIT

# ── 1. writer: populate a diverse corpus exercising every RDB encoding ───────
$RS --port "$WP" --daemonize yes --dir "$DIR" --dbfilename dump.rdb \
    --save '' --appendonly no --logfile "$DIR/writer.log" >/dev/null 2>&1
for _ in $(seq 1 50); do W ping >/dev/null 2>&1 && break; sleep 0.05; done
W flushall >/dev/null
# NB: keep COMPILED defaults on all three servers. The RDB bakes in the encoding
# chosen at SAVE time, but each loader re-evaluates on load, so a non-default
# writer config (e.g. list-max-listpack-size 4) produces a config-default
# false-positive — quicklist in the dump vs listpack after fr's default-config
# load. Exercise quicklist naturally instead, by exceeding the default entry cap.

W set str:embstr "short value" >/dev/null
W set str:raw "$(printf 'x%.0s' {1..120})" >/dev/null
W set str:int 12345 >/dev/null
W set str:int32 70000 >/dev/null
W set str:int64 10000000000 >/dev/null
W set str:lzf "$(printf 'A%.0s' {1..4000})" >/dev/null
W set str:ttl withttl EX 100000 >/dev/null
W rpush list:lp a b c >/dev/null
for i in $(seq 1 500); do W rpush list:ql "AAAAAAAAAAAAAAAAAAAA-$i" >/dev/null; done  # >128 -> quicklist
W hset hash:lp f1 v1 f2 v2 >/dev/null
for i in $(seq 1 300); do W hset hash:ht field$i value$i >/dev/null; done
W sadd set:intset 1 2 3 100 9999 >/dev/null
W sadd set:intset64 9223372036854775807 -9223372036854775808 >/dev/null
W sadd set:lp apple banana cherry >/dev/null
for i in $(seq 1 600); do W sadd set:ht member-$i >/dev/null; done
W zadd zset:lp 1 a 2.5 b 3 c >/dev/null
W zadd zset:float inf pi -inf ni 1e100 big -0 nz >/dev/null
for i in $(seq 1 300); do W zadd zset:sl "$i.5" m$i >/dev/null; done
W xadd stream:s 1-1 a 1 >/dev/null
W xadd stream:s 2-2 b 2 >/dev/null
W xgroup create stream:s grp 0 >/dev/null
W xreadgroup group grp c1 count 1 streams stream:s '>' >/dev/null
# FUNCTION2 opcode at the head of the dump must not abort the whole load
# (regression frankenredis-vd0ii — fr used to drop the entire keyspace).
W function load "#!lua name=problib
redis.register_function('pf', function() return 1 end)" >/dev/null 2>&1

DBSIZE=$(W dbsize)
W save >/dev/null

# ── 2 & 3. reload the identical dump in a fresh redis and in fr ──────────────
cp "$DIR/dump.rdb" "$DIR/fr-dump.rdb"
$RS --port "$OP" --daemonize yes --dir "$DIR" --dbfilename dump.rdb \
    --save '' --appendonly no --logfile "$DIR/oracle.log" >/dev/null 2>&1
"$FR_BIN" --port "$FP" --mode strict --rdb "$DIR/fr-dump.rdb" >"$DIR/fr.log" 2>&1 &
FR_PID=$!
for _ in $(seq 1 100); do O ping >/dev/null 2>&1 && F ping >/dev/null 2>&1 && break; sleep 0.05; done

fails=0
known=0
chk(){ # chk "label" "<expected>" "<actual>"
  if [ "$2" != "$3" ]; then echo "DIVERGE [$1]"; echo "  oracle: $2"; echo "  fr    : $3"; fails=$((fails+1)); fi; }

# ── load sanity ──────────────────────────────────────────────────────────────
chk "dbsize after reload" "$DBSIZE" "$(F dbsize)"
echo "loaded $(F dbsize)/$DBSIZE keys; fr log:"; grep -i "rdb" "$DIR/fr.log" | head -2 | sed 's/^/    /'

# ── per-key deterministic projections (type, encoding, content) ──────────────
strings="str:embstr str:raw str:int str:int32 str:int64 str:lzf str:ttl"
for k in $strings; do chk "GET $k" "$(O get $k|md5sum)" "$(F get $k|md5sum)"; done
chk "TTL str:ttl present" "$([ "$(O ttl str:ttl)" -gt 0 ] && echo y)" "$([ "$(F ttl str:ttl)" -gt 0 ] && echo y)"

for k in str:int str:raw list:lp list:ql hash:lp hash:ht set:intset set:lp set:ht \
         zset:lp zset:sl stream:s; do
  chk "TYPE $k" "$(O type $k)" "$(F type $k)"
  chk "ENCODING $k" "$(O object encoding $k)" "$(F object encoding $k)"
done

for k in list:lp list:ql; do chk "LRANGE $k" "$(O lrange $k 0 -1|md5sum)" "$(F lrange $k 0 -1|md5sum)"; done
for k in hash:lp hash:ht; do chk "HGETALL $k" "$(O hgetall $k|sort|md5sum)" "$(F hgetall $k|sort|md5sum)"; done
for k in set:intset set:intset64 set:lp set:ht; do chk "SMEMBERS $k" "$(O smembers $k|sort|md5sum)" "$(F smembers $k|sort|md5sum)"; done
for k in zset:lp zset:float zset:sl; do chk "ZRANGE $k" "$(O zrange $k 0 -1 withscores|md5sum)" "$(F zrange $k 0 -1 withscores|md5sum)"; done

# ── stream body + group structure (time-relative consumer fields excluded) ───
chk "XRANGE stream:s" "$(O xrange stream:s - +|md5sum)" "$(F xrange stream:s - +|md5sum)"
chk "XINFO GROUPS stream:s" "$(O xinfo groups stream:s|md5sum)" "$(F xinfo groups stream:s|md5sum)"
chk "XPENDING stream:s grp count" "$(O xpending stream:s grp|head -1)" "$(F xpending stream:s grp|head -1)"

# ── consumer seen/active_time round-trip (frankenredis-sq4ov, FIXED) ─────────
# The stream consumer read entries, so `inactive` must be a real value (never
# -1) on both — fr used to drop active_time and show -1 after load.
o_inactive=$(O xinfo consumers stream:s grp | awk '/^inactive$/{getline; print; exit}')
f_inactive=$(F xinfo consumers stream:s grp | awk '/^inactive$/{getline; print; exit}')
chk "XINFO CONSUMERS inactive is-set(active consumer)" \
  "$([ "$o_inactive" != "-1" ] && echo set || echo unset)" \
  "$([ "$f_inactive" != "-1" ] && echo set || echo unset)"

# ── FUNCTION library re-registration (frankenredis-tm139, FIXED) ─────────────
chk "FUNCTION LIST library count" "$(O function list|grep -c library_name)" "$(F function list|grep -c library_name)"

echo "------------------------------------------------------------"
echo "corpus: $DBSIZE keys | hard divergences: $fails | known issues: $known"
if [ "$fails" -eq 0 ]; then
  echo "PASS — fr loads redis 7.2.4 RDB byte-exact across the probed surface"
  exit 0
else
  echo "FAIL — $fails RDB cross-load divergence(s)"
  exit 1
fi
