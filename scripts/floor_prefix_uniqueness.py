#!/usr/bin/env python3
"""Which floor classes share a header prefix, and therefore which header re-checks discriminate?

READ THIS BEFORE USING THE OUTPUT. An arm prologue does TWO things and they have different
safety weight:

  * `strip_prefix(b"*4\r\n$4\r\n")` re-derives the ARITY and the command NAME LENGTH. Both are
    already IMPLIED by the class -- the classifier matches on `(array_len, command)` and
    `command` comes from an exact fixed-length byte-array match -- so this step cannot fail for
    a frame the classifier accepted, and where a prefix is shared it discriminates nothing.
  * `eq_ignore_ascii_case(b"HSET")` re-compares the NAME. **THIS is the class-table backstop**
    (project_floor_arm_prefix_literal_backstops_the_class_table), and trimming an arm prologue
    must preserve it.

So the split below is NOT "where is the lever safe" -- the lever is safe on any arm that keeps
a name comparison. It answers the narrower question of where the HEADER check discriminates
between classes at all.

Method: inside `try_dispatch_floor_classified_action`, map each
`BorrowedDispatchFloorClass::X =>` arm to the `parse_borrowed_plain_*` parser it calls; then
read that parser's own `strip_prefix(b"...")` literal. Two classes sharing a prefix literal
have NO backstop between them and are NOT takeable.
"""
import os
import re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
src = open(os.path.join(ROOT, "crates/fr-server/src/main.rs"),
           encoding="utf-8", errors="replace").read()

# 1. Body of try_dispatch_floor_classified_action.
start = src.find("fn try_dispatch_floor_classified_action(")
assert start != -1
depth, i, end = 0, src.find("{", start), None
for j in range(i, len(src)):
    if src[j] == "{":
        depth += 1
    elif src[j] == "}":
        depth -= 1
        if depth == 0:
            end = j
            break
body = src[i:end]

# 2. class arm -> first parser called inside it
arm_re = re.compile(r"BorrowedDispatchFloorClass::(\w+)(?:\([^)]*\))?\s*=>")
arms = [(m.group(1), m.start()) for m in arm_re.finditer(body)]
arm_parser = {}
for idx, (name, pos) in enumerate(arms):
    stop = arms[idx + 1][1] if idx + 1 < len(arms) else len(body)
    m = re.search(r"(parse_borrowed_plain_\w+)\s*\(", body[pos:stop])
    if m:
        arm_parser.setdefault(name, m.group(1))

# 3. parser -> its header prefix literal
def prefix_of(fn):
    m = re.search(r"fn %s\b" % re.escape(fn), src)
    if not m:
        return None
    window = src[m.start(): m.start() + 4000]
    p = re.search(r'strip_prefix\(b"([^"]+)"\)', window)
    return p.group(1) if p else None

prefix_map = {}
for cls, fn in arm_parser.items():
    pre = prefix_of(fn)
    if pre:
        prefix_map.setdefault(pre, []).append((cls, fn))

print("classified arms inspected: %d, of which %d resolve to a parser with a prefix literal\n"
      % (len(arms), sum(len(v) for v in prefix_map.values())))

unique = {p: v for p, v in prefix_map.items() if len(v) == 1}
shared = {p: v for p, v in prefix_map.items() if len(v) > 1}

print("HEADER PREFIXES CLAIMED BY EXACTLY ONE CLASS (header alone identifies it): %d"
      % len(unique))
for p, v in sorted(unique.items())[:14]:
    print("   %-16s %s" % (p.replace("\\r\\n", "|"), v[0][0]))

print("\nHEADER PREFIXES SHARED BY 2+ CLASSES (header discriminates nothing; the NAME check does): %d"
      % len(shared))
for p, v in sorted(shared.items()):
    print("   %-16s %s" % (p.replace("\\r\\n", "|"), ", ".join(c for c, _ in v)))
