#!/usr/bin/env python3
"""Differential gate: GEO exact precision + ADD flags (frankenredis-...).

GEO is float-precision-sensitive: GEOPOS returns 17-significant-digit lon/lat from the
52-bit interleaved geohash; GEOHASH returns the canonical 11-char base32 string;
GEODIST is haversine with km/m/mi/ft unit conversion. A 1-ULP error anywhere diverges.
This pins exact GEOPOS/GEOHASH/GEODIST (all units, incl. an antipodal-ish long
distance), GEOSEARCH WITHCOORD/WITHDIST/WITHHASH, and GEOADD NX/XX/CH semantics +
post-CH re-encode precision, byte-exact vs redis 7.2.4. (geo_boundary_gate covers
GEOSEARCH boundaries only; this covers the precision + flag surface.)

Usage: geo_precision_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact, 1 = divergence.
"""
import socket, sys, time
def conn(p): return socket.create_connection(("127.0.0.1",p),timeout=5)
def cmd(s,*a):
    o=b"*%d\r\n"%len(a)
    for x in a: x=x if isinstance(x,bytes) else str(x).encode(); o+=b"$%d\r\n%s\r\n"%(len(x),x)
    s.sendall(o); time.sleep(0.02); return s.recv(1<<20)
def main():
    op=int(sys.argv[1]) if len(sys.argv)>1 else 16399
    fp=int(sys.argv[2]) if len(sys.argv)>2 else 16400
    od,fr=conn(op),conn(fp); fails=[]
    for s in (od,fr):
        cmd(s,"FLUSHALL")
        cmd(s,"GEOADD","G","13.361389","38.115556","Palermo")
        cmd(s,"GEOADD","G","15.087269","37.502669","Catania")
        cmd(s,"GEOADD","G","2.349014","48.864716","Paris")
        cmd(s,"GEOADD","G","-122.4194","37.7749","SF")
        cmd(s,"GEOADD","G","0","0","Null")
        cmd(s,"GEOADD","G","-180","85.05112878","Edge")
    def chk(label,*c):
        ro,rf=cmd(od,*c),cmd(fr,*c)
        if ro!=rf: fails.append(f"{label}: redis={ro[:80]!r} fr={rf[:80]!r}")
    chk("geopos_single","GEOPOS","G","Palermo")
    chk("geopos_multi","GEOPOS","G","Catania","Paris","SF","Null","Edge","Nope")
    chk("geohash","GEOHASH","G","Palermo","Catania","Null","Edge","SF")
    chk("geodist_m","GEODIST","G","Palermo","Catania")
    chk("geodist_km","GEODIST","G","Palermo","Catania","km")
    chk("geodist_mi","GEODIST","G","Palermo","Catania","mi")
    chk("geodist_ft","GEODIST","G","Palermo","Catania","ft")
    chk("geodist_long","GEODIST","G","Palermo","SF","km")
    chk("geodist_same","GEODIST","G","Catania","Catania")
    chk("geodist_missing","GEODIST","G","Palermo","Nope")
    chk("geosearch_radius","GEOSEARCH","G","FROMMEMBER","Palermo","BYRADIUS","200","km","ASC","WITHCOORD","WITHDIST","WITHHASH")
    chk("geosearch_box","GEOSEARCH","G","FROMLONLAT","15","37","BYBOX","400","400","km","ASC","WITHDIST")
    chk("georadius_legacy","GEORADIUS","G","15","37","200","km","ASC","WITHDIST","WITHCOORD","WITHHASH")
    chk("georadiusbymember","GEORADIUSBYMEMBER","G","Palermo","200","km","ASC")
    chk("geoadd_nx_existing","GEOADD","G","NX","13.361389","38.115556","Palermo")
    chk("geoadd_xx_new","GEOADD","G","XX","1","1","Brand")
    chk("geoadd_ch","GEOADD","G","CH","99","1","Palermo")
    chk("geopos_after_ch","GEOPOS","G","Palermo")
    print("="*60)
    if fails:
        print(f"FAIL — {len(fails)} GEO precision divergence(s) vs redis 7.2.4:")
        for x in fails[:12]: print(f"  {x}")
        sys.exit(1)
    print("PASS — GEO exact precision + ADD flags byte-exact vs redis 7.2.4 (GEOPOS 17-digit, GEOHASH strings, GEODIST units, WITHHASH, NX/XX/CH)")
if __name__=="__main__": main()
