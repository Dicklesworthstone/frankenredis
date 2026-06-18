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
import socket, sys, time, re
from collections import Counter
def conn(p):
    s=socket.create_connection(("127.0.0.1",p),timeout=5); s.settimeout(1.0); return s
def send(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o)
def rd(s,settle=0.12):
    time.sleep(settle)
    try: return s.recv(1<<20)
    except Exception: return b""
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
    ctl=conn(p); send(ctl,"CONFIG","SET","notify-keyspace-events","KEA"); rd(ctl)
    send(ctl,"FLUSHALL"); rd(ctl)
    for pre in PRE: send(ctl,*pre); rd(ctl,0.02)
    sub=conn(p); send(sub,"PSUBSCRIBE","__keyevent@0__:*"); rd(sub)
    run=conn(p)
    for op in OPS: send(run,*op); rd(run,0.03)
    blob=rd(sub,0.3)
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
