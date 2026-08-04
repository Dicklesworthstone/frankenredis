#!/usr/bin/env python3
"""Differential gate: HSCAN/SSCAN/ZSCAN across encodings (frankenredis-phpcq).

SCAN-family cursor behavior depends on the collection encoding: a SMALL (listpack /
intset) hash/set/zset returns ALL elements in a single cursor=0 reply (byte-exact incl.
order); a LARGE (hashtable / skiplist) collection is cursored, and a FULL iteration
must visit every element exactly once. fr's SCAN uses a deliberately different cursor
scheme (sorted-index) than redis's (reverse-binary), so PER-STEP results legitimately
differ — only the small-collection single-shot reply (compared byte-exact) and the
FULL-iteration element set (compared order-insensitive) are guaranteed equal. Also
pins MATCH filtering, NOVALUES rejected on 7.2.4 (syntax error), SCAN TYPE filter,
missing-key (cursor 0 / empty), and bad-cursor error. vs redis 7.2.4.

Usage: scan_encoding_cursor_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact (per the above contract), 1 = divergence.
"""
import socket, sys, time, re
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def full_iter(s, cmdname, key, *opts):
    """Iterate cursor to 0, return sorted multiset of all returned elements."""
    cur=b"0"; items=[]; guard=0
    while True:
        guard+=1
        if guard>10000: return ("LOOP",)
        r=cmd(s,cmdname,key,cur,*opts)
        m=re.match(rb"\*2\r\n\$\d+\r\n([^\r]*)\r\n",r)
        if not m: return ("PARSE_ERR", r[:60])
        cur=m.group(1)
        bulks=re.findall(rb"\$\d+\r\n([^\r]*)\r\n",r)
        items.extend(bulks[1:])  # drop the cursor bulk
        if cur==b"0": break
    return tuple(sorted(items))

def scan_page(s, cmdname, key, cursor, *opts):
    """Return a SCAN-family cursor and the bulk payload from one small page."""
    reply = cmd(s, cmdname, key, cursor, *opts)
    match = re.match(rb"\*2\r\n\$\d+\r\n([^\r]*)\r\n", reply)
    if not match:
        return None, ("PARSE_ERR", reply[:60])
    payload = re.findall(rb"\$\d+\r\n([^\r]*)\r\n", reply)[1:]
    return match.group(1), payload

def scan_survivors_after_deleting_first_page(s, kind):
    """Return present-throughout members missing from a mid-scan-delete walk."""
    command, key, prefix, delete_command, paired = {
        "hash": ("HSCAN", "surviveh", b"f", "HDEL", True),
        "set": ("SSCAN", "survives", b"m", "SREM", False),
        "zset": ("ZSCAN", "survivez", b"z", "ZREM", True),
    }[kind]
    cursor, payload = scan_page(s, command, key, "0", "COUNT", "10")
    if cursor is None or not payload:
        return ("FIRST_PAGE", payload)
    first_members = payload[::2] if paired else payload
    removed = first_members[:2]
    if len(removed) != 2:
        return ("SHORT_FIRST_PAGE", payload)
    cmd(s, delete_command, key, *removed)
    seen = set(first_members)
    guard = 0
    while cursor != b"0":
        guard += 1
        if guard > 10000:
            return ("LOOP",)
        cursor, payload = scan_page(s, command, key, cursor, "COUNT", "10")
        if cursor is None:
            return payload
        members = payload[::2] if paired else payload
        seen.update(members)
    expected = {prefix + f"{index:04}".encode() for index in range(300)}
    return tuple(sorted((expected - set(removed)) - seen))
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    for s in (od,fr):
        cmd(s,"FLUSHALL")
        for c in ["hash-max-listpack-entries:128","set-max-listpack-entries:128","set-max-intset-entries:512","zset-max-listpack-entries:128"]:
            k,v=c.split(":"); cmd(s,"CONFIG","SET",k,v)
        cmd(s,"HSET","smallh","f1","v1","f2","v2","f3","v3")
        cmd(s,"SADD","smalls","a","b","c"); cmd(s,"SADD","intset","1","2","3")
        cmd(s,"ZADD","smallz","1","a","2","b","3","c")
        for i in range(300): cmd(s,"HSET","bigh",f"f{i}",f"v{i}")
        for i in range(300): cmd(s,"SADD","bigs",f"m{i}")
        for i in range(300): cmd(s,"ZADD","bigz",str(i),f"z{i}")
        for i in range(300): cmd(s,"HSET","surviveh",f"f{i:04}","v")
        for i in range(300): cmd(s,"SADD","survives",f"m{i:04}")
        for i in range(300): cmd(s,"ZADD","survivez",str(i),f"z{i:04}")
    def raw(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        if ro!=rf: fails.append(f"{label}: redis={ro[:70]!r} fr={rf[:70]!r}")
    def iter_set(label,cmdname,key,*opts):
        eo,ef=full_iter(od,cmdname,key,*opts),full_iter(fr,cmdname,key,*opts)
        if eo!=ef: fails.append(f"{label}: redis={str(eo)[:60]} fr={str(ef)[:60]}")
    # small single-shot byte-exact
    raw("hscan_small","HSCAN","smallh","0"); raw("sscan_small","SSCAN","smalls","0")
    raw("sscan_intset","SSCAN","intset","0"); raw("zscan_small","ZSCAN","smallz","0")
    raw("hscan_match","HSCAN","smallh","0","MATCH","f[12]"); raw("sscan_match","SSCAN","smalls","0","MATCH","[ab]")
    # big full-iteration set-equal (per-step cursor scheme differs by design)
    iter_set("hscan_big","HSCAN","bigh"); iter_set("hscan_big_count","HSCAN","bigh","COUNT","10")
    iter_set("sscan_big","SSCAN","bigs"); iter_set("zscan_big","ZSCAN","bigz","COUNT","7")
    iter_set("hscan_big_match","HSCAN","bigh","MATCH","f1*")
    # Redis's SCAN guarantee: a member that remains present throughout an
    # iteration must appear at least once, even when already-returned members
    # are deleted between cursor pages. This exercises the continuation point
    # after HDEL's ordered removal and SREM's swap removal.
    for kind in ("hash", "set", "zset"):
        for server, label in ((od, "redis"), (fr, "fr")):
            missing = scan_survivors_after_deleting_first_page(server, kind)
            if missing:
                fails.append(f"{kind}_delete_survivors_{label}: missing={missing[:6]!r}")
    # error / edge cases
    raw("hscan_novalues","HSCAN","smallh","0","NOVALUES")
    raw("hscan_missing","HSCAN","nope","0"); raw("hscan_badcursor","HSCAN","smallh","abc")
    raw("scan_type","SCAN","0","TYPE","hash","COUNT","10000")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} scan-encoding divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        od.close(); fr.close()
        sys.exit(1)
    od.close(); fr.close()
    print("PASS — HSCAN/SSCAN/ZSCAN across encodings byte-exact vs redis 7.2.4 (small single-shot + big full-iter set + deletion survivors + MATCH/NOVALUES/TYPE/missing/bad-cursor)")
if __name__=="__main__": main()
