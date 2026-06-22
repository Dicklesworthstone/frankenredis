#!/usr/bin/env bash
# (frankenredis-ym6ih) Live DEBUG DIGEST-VALUE parity for HDEL/SREM/SMOVE on
# hashtable-range hash/set, patched fr vs Redis 7.2.4. Identical populate +
# identical delete sequence => digests must match byte-for-byte.
set -u
FR=${FR:-/tmp/fr_patched_ym6ih}
RD=/tmp/redis-oracle-build/redis-7.2.4/src/redis-server
CLI=/data/projects/frankenredis/legacy_redis_code/redis/src/redis-cli
cleanup(){ pkill -f "port 17901" 2>/dev/null; pkill -f "port 17902" 2>/dev/null; sleep 0.3; }
trap cleanup EXIT; cleanup
printf 'enable-debug-command yes\nsave ""\nappendonly no\n' > /tmp/rd_ym6ih.conf
taskset -c 2 "$FR" --port 17901 --enable-debug-command local >/tmp/fr_dig.log 2>&1 &
taskset -c 3 "$RD" /tmp/rd_ym6ih.conf --port 17902 >/tmp/rd_dig.log 2>&1 &
sleep 1.5
for p in 17901 17902; do $CLI -p $p ping >/dev/null || { echo "down $p"; cat /tmp/fr_dig.log; exit 1; }; done

N=5000
populate(){ # port
  { for ((i=0;i<N;i++)); do echo "HSET h field:$(printf %08d $i) val$i"; done
    for ((i=0;i<N;i++)); do echo "SADD s member:$(printf %08d $i)"; done
    # second set for SMOVE source/dest
    for ((i=0;i<N;i++)); do echo "SADD src member:$(printf %08d $i)"; done; } | $CLI -p "$1" --pipe >/dev/null
}
mutate(){ # port  -- identical delete sequence
  { for ((i=0;i<N;i+=2)); do echo "HDEL h field:$(printf %08d $i)"; done       # even fields
    for ((i=1;i<N;i+=3)); do echo "SREM s member:$(printf %08d $i)"; done       # every 3rd member
    for ((i=0;i<N;i+=5)); do echo "SMOVE src s member:$(printf %08d $i)"; done; } | $CLI -p "$1" --pipe >/dev/null
}
for p in 17901 17902; do $CLI -p $p flushall >/dev/null; populate $p; mutate $p; done

fr_h=$($CLI -p 17901 debug digest-value h); rd_h=$($CLI -p 17902 debug digest-value h)
fr_s=$($CLI -p 17901 debug digest-value s); rd_s=$($CLI -p 17902 debug digest-value s)
fr_src=$($CLI -p 17901 debug digest-value src); rd_src=$($CLI -p 17902 debug digest-value src)
echo "HLEN  fr=$($CLI -p 17901 hlen h) rd=$($CLI -p 17902 hlen h)   SCARD s fr=$($CLI -p 17901 scard s) rd=$($CLI -p 17902 scard s)   SCARD src fr=$($CLI -p 17901 scard src) rd=$($CLI -p 17902 scard src)"
echo "h   : fr=$fr_h rd=$rd_h  $( [ "$fr_h" = "$rd_h" ] && echo MATCH || echo DIFF )"
echo "s   : fr=$fr_s rd=$rd_s  $( [ "$fr_s" = "$rd_s" ] && echo MATCH || echo DIFF )"
echo "src : fr=$fr_src rd=$rd_src  $( [ "$fr_src" = "$rd_src" ] && echo MATCH || echo DIFF )"
if [ "$fr_h" = "$rd_h" ] && [ "$fr_s" = "$rd_s" ] && [ "$fr_src" = "$rd_src" ]; then echo "ALL-MATCH"; else echo "MISMATCH"; fi
