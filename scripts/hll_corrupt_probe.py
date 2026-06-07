#!/usr/bin/env python3
"""Differential probe of layered HLL corruption handling (PFADD/PFCOUNT/PFMERGE)
vs redis 7.2.4. fr=:17381 oracle=:17380.

redis isHLLObjectOrReply gates on header only (magic HYLL, len>=16, encoding in
{0,1}, dense=>exact len). Deeper opcode/register corruption is surfaced
*per-command*: PFADD may tolerate, PFCOUNT/PFMERGE may emit INVALIDOBJ.
"""
import socket, sys

def conn(port):
    s = socket.create_connection(("127.0.0.1", port))
    s.settimeout(3)
    return s

def cmd(s, *args):
    buf = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str):
            a = a.encode()
        buf += b"$%d\r\n%s\r\n" % (len(a), a)
    s.sendall(buf)
    return read_reply(s)

def read_reply(s):
    line = read_line(s)
    t, rest = line[:1], line[1:]
    if t == b"+":
        return ("+", rest.decode())
    if t == b"-":
        return ("-", rest.decode())
    if t == b":":
        return (":", int(rest))
    if t == b"$":
        n = int(rest)
        if n == -1:
            return ("$", None)
        data = b""
        while len(data) < n + 2:
            data += s.recv(n + 2 - len(data))
        return ("$", data[:n])
    if t == b"*":
        n = int(rest)
        if n == -1:
            return ("*", None)
        return ("*", [read_reply(s) for _ in range(n)])
    return ("?", line)

def read_line(s):
    buf = b""
    while not buf.endswith(b"\r\n"):
        ch = s.recv(1)
        if not ch:
            break
        buf += ch
    return buf[:-2]

def norm(r):
    # normalize for comparison: error -> just the error code word + class
    t, v = r
    if t == "-":
        # keep full text
        return ("-", v)
    return (t, v)

R = conn(17380)
F = conn(17381)

def both(*args):
    a = cmd(R, *args)
    b = cmd(F, *args)
    return a, b

def show(label, a, b):
    same = norm(a) == norm(b)
    flag = "OK " if same else "DIFF"
    print(f"[{flag}] {label}\n        oracle={a!r}\n        fr    ={b!r}")
    return same

def get_bytes(port, key):
    s = R if port == 17380 else F
    t, v = cmd(s, "GET", key)
    return v

diffs = 0

# Build a valid sparse HLL on the oracle, fetch its raw bytes.
cmd(R, "FLUSHALL"); cmd(F, "FLUSHALL")
cmd(R, "PFADD", "src", "a", "b", "c")
valid_sparse = get_bytes(17380, "src")
print(f"valid sparse HLL len={len(valid_sparse)} head={valid_sparse[:16]!r}")

# Build a valid dense HLL.
cmd(R, "DEL", "srcd")
cmd(R, "PFADD", "srcd", *[str(i) for i in range(2000)])
# force dense via PFDEBUG TODENSE
cmd(R, "PFDEBUG", "TODENSE", "srcd")
valid_dense = get_bytes(17380, "srcd")
print(f"valid dense HLL len={len(valid_dense)} encbyte={valid_dense[4]}")

def case(label, raw):
    global diffs
    cmd(R, "FLUSHALL"); cmd(F, "FLUSHALL")
    cmd(R, "SET", "h", raw); cmd(F, "SET", "h", raw)
    # PFADD onto the corrupt key
    a, b = both("PFADD", "h", "x", "y")
    if not show(f"{label} :: PFADD", a, b): diffs += 1
    # state after PFADD may diverge; reset for a clean PFCOUNT
    cmd(R, "SET", "h", raw); cmd(F, "SET", "h", raw)
    a, b = both("PFCOUNT", "h")
    if not show(f"{label} :: PFCOUNT", a, b): diffs += 1
    # PFMERGE corrupt source into fresh dest
    cmd(R, "SET", "h", raw); cmd(F, "SET", "h", raw)
    cmd(R, "DEL", "dst"); cmd(F, "DEL", "dst")
    a, b = both("PFMERGE", "dst", "h")
    if not show(f"{label} :: PFMERGE(src=h)", a, b): diffs += 1

ba = bytearray

# 1. Truncated sparse: drop the last opcode byte (passes header gate; opcodes sum < 16384)
t = ba(valid_sparse); t = bytes(t[:-1])
case("sparse-truncated-1byte", t)

# 2. Sparse with trailing garbage VAL opcodes appended (opcodes overflow > 16384)
t = ba(valid_sparse) + ba([0b11111111] * 8)  # extra VAL opcodes
case("sparse-overflow-trailing-VAL", bytes(t))

# 3. Sparse with header intact but body replaced by a single XZERO that under-fills
#    XZERO opcode: 01xxxxxx yyyyyyyy -> (xxxxxxyyyyyyyy)+1 zero registers. Use 0x40 0x00 -> 1 reg.
t = ba(valid_sparse[:16]) + ba([0x40, 0x00])
case("sparse-underfill-1reg", bytes(t))

# 4. Header gate failures (should be WRONGTYPE on ALL commands)
# 4a. bad magic
t = ba(valid_sparse); t[0] = ord('X')
case("badmagic", bytes(t))
# 4b. bad encoding byte (encoding=5)
t = ba(valid_sparse); t[4] = 5
case("badenc", bytes(t))
# 4c. too short (<16)
case("tooshort", b"HYLL" + b"\x00" * 5)
# 4d. dense encoding but wrong length
t = ba(valid_dense); t = bytes(t[:-1])  # dense len != HLL_DENSE_SIZE
case("dense-wronglen", t)

# 5. non-HLL type entirely (a list) -> plain WRONGTYPE
cmd(R, "FLUSHALL"); cmd(F, "FLUSHALL")
cmd(R, "RPUSH", "h", "x"); cmd(F, "RPUSH", "h", "x")
a, b = both("PFCOUNT", "h")
if not show("list-key :: PFCOUNT", a, b): diffs += 1
a, b = both("PFADD", "h", "z")
if not show("list-key :: PFADD", a, b): diffs += 1

# 6. dense with a register value > 50 (still structurally fine; redis tolerates) - sanity
t = ba(valid_dense)
case("dense-valid-roundtrip", bytes(t))

print(f"\n==== TOTAL DIFFS: {diffs} ====")
sys.exit(1 if diffs else 0)
