#!/bin/bash
# Stream RDB round-trip parity. Streams carry consumer groups and PEL state, so
# content equality is not enough: XINFO STREAM's counters, XINFO GROUPS and
# XPENDING all have to survive DEBUG RELOAD identically.
set -u
CLI=/data/projects/frankenredis/legacy_redis_code/redis/src/redis-cli
W=/data/tmp/claude-1000/frx-bt/sgate
KEYS="s_plain s_groups s_pending s_tomb s_auto s_many s_wide s_one"

seed () {
  local p="$1"
  # plain stream, explicit IDs
  for i in 1 2 3 4 5; do "$CLI" -p "$p" XADD s_plain "$i-1" f "v$i" > /dev/null; done
  # a stream with two consumer groups, nothing read yet
  for i in 1 2 3; do "$CLI" -p "$p" XADD s_groups "$i-1" f "v$i" > /dev/null; done
  "$CLI" -p "$p" XGROUP CREATE s_groups g1 0 > /dev/null
  "$CLI" -p "$p" XGROUP CREATE s_groups g2 '$' > /dev/null
  # a group with PENDING entries: read but never ack -> PEL must survive
  for i in 1 2 3 4; do "$CLI" -p "$p" XADD s_pending "$i-1" f "v$i" > /dev/null; done
  "$CLI" -p "$p" XGROUP CREATE s_pending gp 0 > /dev/null
  "$CLI" -p "$p" XREADGROUP GROUP gp alice COUNT 3 STREAMS s_pending '>' > /dev/null
  "$CLI" -p "$p" XACK s_pending gp 1-1 > /dev/null
  # tombstones: delete from the middle, so max-deleted-id and entries-added split
  for i in 1 2 3 4 5 6; do "$CLI" -p "$p" XADD s_tomb "$i-1" f "v$i" > /dev/null; done
  "$CLI" -p "$p" XDEL s_tomb 3-1 4-1 > /dev/null
  # auto-generated IDs
  for i in 1 2 3; do "$CLI" -p "$p" XADD s_auto '*' f "v$i" > /dev/null; done
  # enough entries to cross a listpack node boundary
  for i in $(seq 1 300); do "$CLI" -p "$p" XADD s_many "$i-1" f "v$i" > /dev/null; done
  # many fields in one entry
  "$CLI" -p "$p" XADD s_wide 1-1 a 1 b 2 c 3 d 4 e 5 f 6 g 7 h 8 > /dev/null
  # single entry
  "$CLI" -p "$p" XADD s_one 7-7 only 1 > /dev/null
}

report () {
  local p="$1" when="$2"
  for k in $KEYS; do
    printf "  %-9s %s len=%-4s content=%s info=%s groups=%s pend=%s\n" "$k" "$when" \
      "$("$CLI" -p "$p" XLEN $k)" \
      "$("$CLI" -p "$p" XRANGE $k - + | md5sum | cut -c1-10)" \
      "$("$CLI" -p "$p" XINFO STREAM $k | md5sum | cut -c1-10)" \
      "$("$CLI" -p "$p" XINFO GROUPS $k | md5sum | cut -c1-10)" \
      "$("$CLI" -p "$p" XPENDING $k gp 2>/dev/null | md5sum | cut -c1-10)"
  done
}

arm () {
  local bin="$1" port="$2" label="$3"
  local dir="$W/$label"; mkdir -p "$dir"
  "$bin" --port "$port" --save '' --appendonly no --dir "$dir" \
         --enable-debug-command yes > /dev/null 2>&1 &
  local pid=$!
  for _ in $(seq 1 60); do "$CLI" -p "$port" PING > /dev/null 2>&1 && break; sleep 1; done
  seed "$port"
  echo "$label"
  report "$port" "before"
  "$CLI" -p "$port" DEBUG RELOAD > /dev/null
  report "$port" "after "
  echo "  digest: $("$CLI" -p "$port" DEBUG DIGEST)"
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
}

rm -rf "$W" 2>/dev/null; mkdir -p "$W"
arm "$1" 48101 "fr CONTROL"
[ $# -ge 2 ] && arm "$2" 48102 "fr CANDIDATE"
arm /data/projects/frankenredis/legacy_redis_code/redis/src/redis-server 48103 "redis 7.2.4"
