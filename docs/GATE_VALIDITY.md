# Gate validity: how a differential gate passes without testing anything

A differential gate compares frankenredis against vendored redis 7.2.4 and
prints PASS or FAIL. The failure mode that matters is not FAIL — that gets
investigated. It is **PASS for a reason unrelated to the property under test**.
Those survive review indefinitely, because the output looks exactly like
success.

Six mechanisms have been measured on real gates in this repo. All six produced a
green gate. **None of them was a product bug** — in every case frankenredis
already matched redis and only the test was wrong. That asymmetry is the reason
this document exists: when a gate looks suspicious, audit the gate first.

## The six mechanisms

### 1. Truncating read — `sleep(); recv()`

`s.sendall(cmd); time.sleep(0.02); return s.recv(1 << 20)` returns whatever has
**arrived**, not a complete RESP frame.

*Measured*: a 4000-member `ZRANGEBYLEX` came back as exactly 65536 bytes of a
~96 KB reply.

*Why it passes*: both sides of a differ use the same reader, so two replies
truncated at the same offset **compare equal while differing past the cut**.

*Detect*: `grep -l 'return.*\.recv('`. It only bites when the script also issues
a bulk readback (`LRANGE`/`HGETALL`/`DUMP`/…) over a large fixture.

*Fix*: `from _respread import cmd` — reads until a whole frame is buffered.

### 2. Fixture re-encoding — bare `.encode()`

A fixture written as a str escape (`"\xff"`) sent through a bare `.encode()`
becomes UTF-8: `0xff` → `0xc3 0xbf`, which is **valid UTF-8**.

*Measured*: an "all-0xff" `BITPOS` fixture was actually `c3 bf c3 bf`, whose
first zero bit is at index 2 — so the all-ones rule the gate existed to pin was
never exercised. Two other gates written specifically to test non-UTF8 handling
were sending valid UTF-8.

*Why it passes*: both engines receive the same wrong bytes.

*Detect*: AST walk for a bare `.encode()` plus a str constant containing a
codepoint in `0x80..0xFF`. Plain grep over-reports — `b"\xff"` is safe.

*Fix*: `_respread.encode_arg` (latin-1). **Exception**: fixtures that are real
*text* rather than byte escapes — accented words in a collation gate — should
stay UTF-8, because that is what a real client sends.

### 3. Unseeded fixture

The cases search for data the seed never contained.

*Measured*: lex bounds of `"[\xff"` against an all-ASCII member set. Matches
nothing on either engine.

*Why it passes*: both sides return empty.

*Fix*: seed the bytes the cases look for, and assert they landed.

### 4. Unverified seed

Seeding fails silently and nothing notices.

*Why it passes*: most commands answer `0` / `-1` / empty for a **missing key**
on both engines. In a STORE-family gate this is especially perverse — an
unseeded run makes every "empty result deletes dest" assertion trivially true,
satisfied by the very failure it exists to detect.

*Fix*: `_respread.assert_seed(reply, expected, label)` after every seed.

### 5. State verified by the wrong command

The readback cannot distinguish the states the clause is about.

*Measured*: `LRANGE` returns `*0` for both a **deleted key** and a **present but
empty list**, so a "LTRIM empties-key deletes the key" clause asserted nothing.

*Fix*: compare `EXISTS` and `TYPE` as well when the clause is about existence,
type or encoding.

### 6. Documentation drift

The docstring describes coverage the code does not implement.

*Measured*: a STORE gate's docstring claimed it seeded "dest keys of a string +
list type"; `reset()` only ever ran `SET dest`, so cross-type overwrite was only
ever string→set.

*Why it is the worst of the six*: nothing runs, so no amount of green tells you
about it — and an auditor checking a gate against its own documentation (a
reasonable thing to do) is actively misled.

*Fix*: when auditing, verify each documented case against the code that sets it
up, not against the docstring.

## Before you add a gate

1. Does one already exist? Several beads asking for "a differential gate" were
   satisfied by a gate that had existed for months.
2. Read the whole file, not a sample. Two errors this session came from reading
   part of a test and concluding wrongly.
3. Run it, then **break it on purpose** in a scratch copy and confirm it fails.
   A gate that cannot fail is indistinguishable from a correct one on a green
   run. Invert a comparison, or reintroduce the defect you just fixed.
4. Check the seed lands, and that the fixtures contain the bytes the cases
   search for.
5. Check the readback can observe the property — `EXISTS`/`TYPE`, not just a
   range read.
6. Use `scripts/_respread.py` rather than hand-rolling a client. It was
   extracted after the same fix was hand-copied into six gates and regressed in
   the seventh.
