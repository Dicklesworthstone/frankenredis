#!/usr/bin/env bash
# differential_probe.sh — live byte-exact parity check of fr-server vs the
# vendored redis 7.2.4 oracle. A turnkey regression harness: run it after any
# change to the command/value/encoding surface (e.g. the SmallStr/SSO refactor)
# to confirm fr still matches upstream byte-for-byte.
#
# SETUP (both servers must use COMPILED defaults so configs align — start the
# oracle config-LESS; a shipped redis.conf sets hash/list-max-listpack=128 while
# fr uses the compiled 512/-2, a known false-positive class):
#   ORACLE=legacy_redis_code/redis/src
#   $ORACLE/redis-server --port 16399 --daemonize yes --save '' --appendonly no
#   # build fr locally (CARGO_TARGET_DIR is /data/tmp/cargo-target here):
#   cargo build -p fr-server   # binary: $CARGO_TARGET_DIR/debug/frankenredis
#   $CARGO_TARGET_DIR/debug/frankenredis --port 16400 --mode strict &   # strict!
#   scripts/differential_probe.sh 16399 16400
#
# KNOWN non-bug divergences (filtered out — do NOT report as failures):
#   - INCRBYFLOAT/HINCRBYFLOAT >17-digit results: redis uses 80-bit long double,
#     fr uses f64 (WONTFIX, needs f80).
#   - SCAN/HSCAN/SSCAN/ZSCAN element ORDER: fr iterates a BTreeSet, redis a dict
#     bucket array — order is unspecified by the SCAN contract.
#   - RANDOMKEY / SRANDMEMBER / *RANDMEMBER picks, client ids, and dynamic INFO
#     values (uptime/pid/memory/run_id) — inherently nondeterministic.
set -u
OP=${1:-16399}; FP=${2:-16400}
CLI=${CLI:-legacy_redis_code/redis/src/redis-cli}
O(){ $CLI -p "$OP" "$@" 2>&1; }
F(){ $CLI -p "$FP" "$@" 2>&1; }
fails=0
# diff one command; $* are redis args
t(){ local o f; o=$(O "$@"|tr '\n' '|'); f=$(F "$@"|tr '\n' '|')
     if [ "$o" != "$f" ]; then echo "DIVERGE [$*]"; echo "  oracle: $o"; echo "  fr    : $f"; fails=$((fails+1)); fi; }

O flushall >/dev/null; F flushall >/dev/null

# ── strings / numeric / expiry ────────────────────────────────────────────
O set k 5>/dev/null; F set k 5>/dev/null;       t object encoding k     # int
O set k hello>/dev/null; F set k hello>/dev/null; t object encoding k   # embstr
O set k aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa>/dev/null
F set k aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa>/dev/null; t object encoding k # raw>44
t set k v2 keepttl get; t append k zzz; t setrange k 2 XY; t getrange k 1 3; t getrange k -3 -1
t expire k 100 NX; t expire k 200 XX; t expire k 50 GT; t ttl k; t setex bad 0 v; t lpush k x

# ── lists ─────────────────────────────────────────────────────────────────
O del l>/dev/null;F del l>/dev/null; O rpush l a b c a b c a>/dev/null;F rpush l a b c a b c a>/dev/null
t lpos l a rank -1 count 2; t lpos l a count 0; t lrange l 0 -1; t linsert l before b ZZ
t lrem l -2 a; t object encoding l; t lmpop 2 nope l LEFT COUNT 2

# ── sets / zsets / hashes ──────────────────────────────────────────────────
O del s1 s2>/dev/null;F del s1 s2>/dev/null; O sadd s1 1 2 3 a>/dev/null;F sadd s1 1 2 3 a>/dev/null
O sadd s2 2 3 b>/dev/null;F sadd s2 2 3 b>/dev/null
t object encoding s1; t sintercard 2 s1 s2 limit 1; t sdiff s1 s2; t smismember s1 1 a z
O del z>/dev/null;F del z>/dev/null; O zadd z 0 a 0 b 0 c 0 d 0 e>/dev/null;F zadd z 0 a 0 b 0 c 0 d 0 e>/dev/null
t zrangebylex z "[a" "(c"; t zrangebylex z "(a" "+"; t zrevrangebylex z "+" "-"; t zlexcount z "-" "+"
t zrangebylex z "-" "+" LIMIT 2 2; t zadd z gt ch 5 a; t zrangebyscore z "(0" "+inf" WITHSCORES; t object encoding z
O del h>/dev/null;F del h>/dev/null; O hset h a 1 b 2>/dev/null;F hset h a 1 b 2>/dev/null
t hincrbyfloat h a 1.5; t object encoding h

# ── bitops ─────────────────────────────────────────────────────────────────
O set bk foobar>/dev/null;F set bk foobar>/dev/null
t bitcount bk 1 1 BYTE; t bitcount bk 5 30 BIT; t bitpos bk 1 2 -1 BIT
t bitfield bk get i8 0 set u4 100 15 incrby i5 100 10; t setbit nb 100 1; t getbit nb 100

# ── streams / GEO / scripting ──────────────────────────────────────────────
O xadd st 1-1 a 1>/dev/null;F xadd st 1-1 a 1>/dev/null; O xadd st 2-2 b 2>/dev/null;F xadd st 2-2 b 2>/dev/null
t xlen st; t xrange st - +; t xrevrange st + - COUNT 1; t object encoding st
t xinfo stream st; t xgroup create st g1 0; t xreadgroup group g1 c1 count 5 streams st ">"; t xpending st g1
t geoadd geo 13.361389 38.115556 Palermo 15.087269 37.502669 Catania
t geodist geo Palermo Catania km; t geohash geo Palermo; t geosearch geo FROMMEMBER Palermo BYRADIUS 200 km ASC
t eval "return {1,2,3,'ciao'}" 0; t eval "return redis.error_reply('custom')" 0; t eval "return redis.sha1hex('')" 0

# ── arity / error-wording sweep ────────────────────────────────────────────
for c in getrange setrange setbit bitcount bitop incrby setex getex append hset hincrby \
         linsert lrem lpos lset rpoplpush smove sintercard spop zadd zincrby zrangebylex \
         zrangestore copy object expire sort lmpop xadd xrange pfadd pfcount; do t "$c"; done
t set k v EX notnum; t set k v EX 10 PX 20; t zadd k GT LT 1 m; t zadd k NX GT 1 m
t bitcount k 1 2 ZZZ; t getex k EX 10 PERSIST; t lpos k v RANK 0; t sort k BY

# ── INFO field-name parity (names only; dynamic values ignored) ────────────
for sec in server clients memory persistence stats replication cpu keyspace; do
  diff <(O info $sec|grep -oE '^[a-z_0-9]+:'|sort -u) <(F info $sec|grep -oE '^[a-z_0-9]+:'|sort -u) >/dev/null \
    || { echo "DIVERGE [INFO $sec field-names]"; fails=$((fails+1)); }
done

echo "------------------------------------------------------------"
if [ "$fails" -eq 0 ]; then echo "PASS — fr is byte-exact with redis 7.2.4 across the probed surface"; exit 0
else echo "FAIL — $fails divergence(s) (filter known WONTFIX above)"; exit 1; fi
