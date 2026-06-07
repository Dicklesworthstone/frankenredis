#!/usr/bin/env python3
"""lua_semantics_differ.py — Lua LANGUAGE & pattern-engine parity vs redis 7.2.4.

fr ships a hand-written Lua 5.1 interpreter, so the language core — string
pattern matching (captures, character classes, anchors, %b/%f, gmatch/gsub),
metatable dispatch (__index/__add/__tostring/__len/__eq, rawget/rawset/raw*),
number<->string coercion, string.format conversions, and control flow/closures
— is a deep, bug-prone surface. The companion lua_lib_differ.py covers the
LIBRARIES (cjson/cmsgpack/struct/bit/sha1hex); this gate covers the language
semantics those don't touch, by EVAL'ing each snippet on fr and vendored redis
and comparing the reply (error replies compared by class to ignore harmless
line-number wording).

Usage: lua_semantics_differ.py [--oracle 16399] [--fr 16400]
Exit 0 if every snippet's reply matches, else 1.
"""
import argparse
import socket
import sys


class Conn:
    def __init__(self, port):
        self.s = socket.create_connection(("127.0.0.1", port), 3)
        self.s.settimeout(3.0)
        self.b = b""

    def _line(self):
        while b"\r\n" not in self.b:
            self.b += self.s.recv(65536)
        l, self.b = self.b.split(b"\r\n", 1)
        return l

    def _rn(self, n):
        while len(self.b) < n + 2:
            self.b += self.s.recv(65536)
        d, self.b = self.b[:n], self.b[n + 2:]
        return d

    def parse(self):
        l = self._line()
        t, r = l[:1], l[1:]
        if t == b"+":
            return ("+", r.decode("latin1"))
        if t == b":":
            return (":", int(r))
        if t == b"-":
            return ("-", r.decode("latin1"))
        if t == b"$":
            n = int(r)
            return ("_",) if n < 0 else ("$", self._rn(n).decode("latin1"))
        if t == b"*":
            n = int(r)
            return ("_",) if n < 0 else ("*", [self.parse() for _ in range(n)])
        raise ValueError(l)

    def cmd(self, *a):
        out = b"*%d\r\n" % len(a)
        for x in a:
            x = x if isinstance(x, bytes) else str(x).encode()
            out += b"$%d\r\n%s\r\n" % (len(x), x)
        self.s.sendall(out)
        return self.parse()


SNIPPETS = [
    # --- string patterns: match / captures / classes / anchors / %b / %f ---
    r"return string.match('hello123world','%d+')",
    r"return {string.match('key=val','(%w+)=(%w+)')}",
    r"return {string.match('2024-01-15','(%d+)-(%d+)-(%d+)')}",
    r"return string.match('  trim  ','^%s*(.-)%s*$')",
    r"return string.match('abc','^a')",
    r"return string.match('abc','c$')",
    r"return string.match('aXbXc','([^X]+)')",
    r"return string.match('(nested)','%b()')",
    r"return string.match('foobar','foo%f[%w]')",
    r"return tostring(string.match('abc','%d'))",
    r"return string.match('a.b.c','%.')",
    r"return string.match('CamelCase','%u%l+')",
    r"return {string.match('hello','(h)(e)(l)')}",
    r"return string.match('test123','%w-(%d+)')",
    # --- gmatch ---
    r"local t={}; for w in string.gmatch('a,b,c','[^,]+') do t[#t+1]=w end return t",
    r"local t={}; for k,v in string.gmatch('a=1;b=2','(%w+)=(%w+)') do t[#t+1]=k..v end return t",
    r"local n=0; for _ in string.gmatch('aaa','a') do n=n+1 end return n",
    # --- gsub: function / table / captures / empty pattern ---
    r"return {string.gsub('hello world','%w+',string.upper)}",
    r"return {string.gsub('aaa','a','b',2)}",
    r"return string.gsub('x=1,y=2','(%w+)=(%w+)','%2=%1')",
    r"return string.gsub('abc','(.)','%1%1')",
    r"return string.gsub('hello','l',{l='L'})",
    r"return (string.gsub('hello','','-'))",
    # --- find with patterns / plain ---
    r"return {string.find('hello123','(%d+)')}",
    r"return {string.find('a+b','+',1,true)}",
    # --- metatables ---
    r"local t=setmetatable({},{__index=function() return 99 end}); return t.anything",
    r"local t=setmetatable({},{__index={x=5}}); return t.x",
    r"local t=setmetatable({v=1},{__add=function(a,b) return a.v+b end}); return t+10",
    r"local t=setmetatable({},{__tostring=function() return 'CUSTOM' end}); return tostring(t)",
    r"local t=setmetatable({},{__len=function() return 42 end}); return #t",
    r"local t={}; rawset(t,'k','v'); return rawget(t,'k')",
    r"local t=setmetatable({},{__index=function() return 1 end}); return rawget(t,'x')==nil and 'nil' or 'x'",
    r"local a={}; local b=a; return rawequal(a,b) and 'y' or 'n'",
    r"local t=setmetatable({},{__eq=function() return true end}); local u=setmetatable({},getmetatable(t)); return t==u and 'y' or 'n'",
    # --- number <-> string coercion ---
    r"return 10 .. 20",
    r"return tostring(3.14)",
    r"return tonumber('0x1A')",
    r"return tonumber('  42  ')",
    r"return tonumber('3.5e2')",
    r"return tonumber('abc') == nil and 'nil' or 'num'",
    r"return tonumber('10','2')",
    r"return 7 % 3",
    r"return tostring(7 / 2)",
    r"return 3 == 3.0 and 'y' or 'n'",
    r"return tostring(2^53)",
    r"return tostring(0/0)",
    r"return tostring(1/0)",
    r"return tostring(math.huge)",
    # --- string.format conversions / precision ---
    r"return string.format('%d-%s-%x',255,'q',255)",
    r"return string.format('%5.2f',3.14159)",
    r"return string.format('%q','a\"b')",
    r"return string.format('%c',65)",
    r"return string.format('%g',100000000)",
    r"return string.format('%g',0.0001)",
    r"return string.format('%e',12345.678)",
    r"return string.format('%.0f',2.5)",
    r"return string.format('%.0f',3.5)",
    r"return string.format('%o',64)",
    r"return string.format('%5d|%-5d|%+d|%05.2f',42,42,42,3.1)",
    r"return string.format('%.17g',0.1)",
    r"return string.format('%d',3.0)",
    # --- string lib (non-pattern) ---
    r"return string.sub('hello',2,4)..string.sub('hello',-3)",
    r"return string.rep('ab',3,'-')",
    r"return {string.byte('ABC',1,3)}",
    r"return string.char(72,73)..string.reverse('abc')",
    r"return string.len('h\xc3\xa9llo')",
    # --- table / select / unpack / next / pairs ---
    r"local t={3,1,2}; table.sort(t); return t",
    r"local t={'b','a','c'}; table.sort(t); return table.concat(t,',')",
    r"local t={1,2,3}; table.remove(t,2); table.insert(t,9); return t",
    r"return select('#',1,2,3)..select(2,'a','b','c')",
    r"return {unpack({10,20,30})}",
    r"local t={a=1,b=2}; local s=0; for k,v in pairs(t) do s=s+v end return s",
    r"local t={10,20,30}; local s=0; for i,v in ipairs(t) do s=s+v end return s",
    r"local t={}; return next(t)==nil and 'empty' or 'no'",
    # --- closures / control flow / varargs / errors ---
    r"local function fib(n) if n<2 then return n end return fib(n-1)+fib(n-2) end return fib(12)",
    r"local s=0; for i=10,1,-2 do s=s+i end return s",
    r"local x=5; local f=function() return x end; x=10; return f()",
    r"return (function(...) return select('#',...) end)(1,2,3,4)",
    r"local ok,e=pcall(function() error('boom') end); return tostring(ok)",
    r"local ok,e=pcall(function() error({code=5}) end); return type(e)",
    r"return 5 and 6",
    r"return nil and 6 or 7",
    r"return not not 0",
    r"return type(2^53)",
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", type=int, default=16399)
    ap.add_argument("--fr", type=int, default=16400)
    args = ap.parse_args()
    R, F = Conn(args.oracle), Conn(args.fr)

    def norm(x):
        # compare error replies by leading code word (line-number wording is noise)
        if x[0] == "-":
            return ("-", x[1].split()[0] if x[1] else "")
        return x

    failures = []
    for snip in SNIPPETS:
        a = R.cmd("EVAL", snip, "0")
        b = F.cmd("EVAL", snip, "0")
        if norm(a) != norm(b):
            failures.append((snip, a, b))
    if failures:
        print(f"FAIL: {len(failures)} Lua semantics divergence(s):")
        for snip, a, b in failures:
            print(f"  {snip!r}\n     redis={a!r}\n     fr   ={b!r}")
        sys.exit(1)
    print(f"OK: {len(SNIPPETS)} Lua language/pattern/metatable snippets match "
          "redis 7.2.4 (string patterns, captures, %b/%f, metatables, number "
          "coercion, string.format, closures)")


if __name__ == "__main__":
    main()
