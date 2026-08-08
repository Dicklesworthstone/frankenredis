#!/usr/bin/env python3
"""Differential gate: per-command keyspace-notification event NAMES (frankenredis-...).

With notify-keyspace-events=KEA, every mutating command publishes a __keyevent@0__:<ev>
notification whose <ev> name must match redis exactly — these are easy to get subtly
wrong (INCR fires `incrby` not `incr`; ZINCRBY -> `zincr`; COPY -> `copy_to`; RENAME ->
`rename_from`+`rename_to`; GETDEL/GETEX-expire -> `del`; LMOVE -> source `lpop|rpop` +
dest `lpush|rpush`). This fires ~28 commands and compares the emitted event-name
multiset (delivery order is unspecified) byte-exact vs redis 7.2.4.

Usage: keyspace_event_name_differ.py <oracle_port> <fr_port>
       Exit 0 = identical event multiset, 1 = divergence.
"""
import re
import sys
import time  # only for drain()'s settle wait, never for reading a command reply
from collections import Counter

from _respread import assert_ok, assert_seed, cmd
from _respread import conn as _conn


def conn(p):
    s = _conn(p)
    s.settimeout(1.0)  # bounds drain()'s wait, not the framed reads
    return s


def drain(s, settle=0.3):
    """Collect the asynchronously-delivered pmessage frames, or confirm none came.

    DELIBERATE EXCEPTION to the shared reader (frankenredis-gpry6): keyspace
    notifications are not replies to anything this connection sent, so frame
    completeness cannot tell the reader how many to expect or when to stop —
    only waiting can. Every OTHER read in this gate is a command reply and goes
    through the shared cmd().
    """
    time.sleep(settle)
    try:
        return s.recv(1 << 20)
    except Exception:
        return b""
PRE=[["RPUSH","mylist","a","b","c"],["SADD","myset","x","y"],["HSET","myhash","f","v"],
     ["ZADD","myzset","1","a"],["SET","str1","v"],["SET","num","5"],["SET","ttl1","v"],
     ["XADD","mystream","1-1","f","v"],["SET","app1","v"],["SET","ren1","v"]]
OPS=[["SET","newk","v"],["APPEND","app1","x"],["SETRANGE","str1","0","Z"],["INCR","num"],
     ["INCRBYFLOAT","num","1.5"],["GETSET","str1","new"],["DEL","newk"],
     ["LPUSH","mylist","z"],["RPOP","mylist"],["LSET","mylist","0","q"],["LREM","mylist","0","q"],
     ["SADD","myset","z"],["SREM","myset","x"],["SPOP","myset"],
     ["HSET","myhash","g","2"],["HDEL","myhash","f"],["HINCRBY","myhash","cnt","1"],
     ["ZADD","myzset","2","b"],["ZINCRBY","myzset","1","a"],["ZREM","myzset","a"],
     ["EXPIRE","ttl1","100"],["PERSIST","ttl1"],["XADD","mystream","2-2","f","v"],
     ["RENAME","ren1","ren2"],["COPY","str1","cp1"],["SETEX","sx","100","v"],
     ["GETDEL","app1"],["LMOVE","mylist","mylist","LEFT","RIGHT"]]
def events(p):
    ctl=conn(p)
    assert_ok(cmd(ctl,"CONFIG","SET","notify-keyspace-events","KEA"), "CONFIG SET notify-keyspace-events")
    assert_ok(cmd(ctl,"FLUSHALL"), "FLUSHALL")
    for pre in PRE: cmd(ctl,*pre)
    # The PRE seeds have per-row reply shapes, so pin the total instead: if they
    # silently failed, every OP below would operate on a missing key and BOTH
    # engines would emit the same (empty) event set — a gate that passes having
    # observed no notifications at all. (frankenredis-gpry6)
    assert_seed(cmd(ctl,"DBSIZE"), len({r[1] for r in PRE}), "PRE seeds present")
    sub=conn(p); cmd(sub,"PSUBSCRIBE","__keyevent@0__:*")   # one confirmation frame
    run=conn(p)
    for op in OPS: cmd(run,*op)
    blob=drain(sub)
    for c in (ctl,sub,run): c.close()
    return Counter(re.findall(rb"__keyevent@0__:([a-z_]+)", blob))
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    co,cf=events(op),events(fp)
    if co!=cf:
        print("="*60)
        print("FAIL — keyspace event-name divergence vs redis 7.2.4:")
        print(f"  MISSING in fr: {sorted((co-cf).items())}")
        print(f"  EXTRA in fr:   {sorted((cf-co).items())}")
        sys.exit(1)
    print("="*60)
    print(f"PASS — per-command keyspace event names byte-exact vs redis 7.2.4 ({sum(co.values())} events across ~28 commands)")
if __name__=="__main__": main()
