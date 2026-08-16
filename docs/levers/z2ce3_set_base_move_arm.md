# set_base — the last untaken rewire. Move the arm, don't add a floor class.

CODE WORK, written at loadavg 79.84 (measurement correctly refused). NOT applied to the
tree: `crates/fr-server/src/main.rs` currently carries a peer's uncommitted arity-6 ZADD
chaining fix, and layering on an uncommitted shared file is what cost this campaign two
levers already. **Re-locate every site by CONTENT at apply time.**

## What is wrong

`set_base` is the only shape on the certified table outside the cheap-to-reach dispatch
band: **33.9 pct, ~723 instr/op**, against 15.6-23.9 pct for every other route and 20.6
for `get_control`. Plain SET is **absent from the floor token table entirely** — not
mis-claimed at the wrong arity like ZADD/EXPIRE/BITCOUNT were, simply never added.

    parser     parse_borrowed_plain_set_packet     cascade arm at 7303
    executor   execute_plain_set_borrowed          fr-runtime
    floor      NOTHING

It is cheap to *walk* to — the seven SET option parsers ahead of it are arity-gated, so a
`*3` packet fails each on the first byte pair — which is exactly why a command paying 2.6x
`get_control`'s dispatch never looked bad enough to notice.

## Two fixes; take (B)

**(A) Add a floor class for `*3` SET.** Carries the `9hnxt` trap: claim `*3` ONLY, or every
`SET NX` / `SET EX` / `SET GET` is claimed by an arm whose parser refuses it and lands on
the GENERIC path — turning seven cheap header compares into a full generic dispatch on
the most common write shapes. A large regression dressed as a win.

**(B) Move SET's cascade arm up beside GET's.** No classifier entry, no arity claim, no
possibility of swallowing the option forms, because they keep their own arms and match on
prefixes base SET does not have. **GET proves first-in-cascade already costs ~275
instr/op**, and GET is not floor-classified either — its arm is simply first.

(B) is smaller, reversible, and carries none of (A)'s failure mode.

## Verified unobstructed — re-checked on current source

Every parser called between GET's arm (6960) and SET's (7303), with its OWN prefix (these
are hardcoded inside the parser bodies, so a literal-scan of the call-site range does NOT
show them — that mistake is recorded in this campaign's ledger):

    keyed_pop            *2/$4        watch      *2/$5        unwatch  *1/$7
    set_nx, set_xx       *4/$3
    set_relexpire, set_opt_get, set_absexpire    *5/$3
    set_relexpire_get, set_cond_relexpire        *6/$3
    key_arg1, key_arg2   prefix is a PARAMETER; the call sites in this span pass
                         *3/$4 and *4/$3

    grep for `b"*3\r\n$3\r\n"` in lines 6960-7303  ->  ZERO

`*3` with a three-character command is exactly plain SET. **Nothing in the span can
intercept it**, so the move cannot change which arm serves any packet — only the order in
which arms are tried.

## The change

Locate by content: the `else if let Some(packet) = parse_borrowed_plain_set_packet(` arm
(currently ~7303). Move that entire `else if` block so it sits immediately after the
`parse_borrowed_plain_get_packet` arm (~6960), preserving its body verbatim.

Add at the moved site:

```rust
// (frankenredis-z2ce3) Base SET sits directly after GET because dispatch cost in
// this cascade is POSITION, not classification: GET is not floor-classified either
// and pays ~275 instr/op purely by being first. SET was at ~7303 paying ~723 — the
// ~450 difference is the walk, and it is spent on the most frequently issued command
// there is.
//
// This is deliberately NOT a floor class. Claiming SET at the floor would require
// claiming `*3` alone; a claim that reached the `*4`/`*5`/`*6` option forms would
// hand each to an arm whose parser refuses it, and a floor decline goes to the
// GENERIC path rather than back to the cascade (frankenredis-9hnxt). Moving the arm
// has no such failure mode: the option parsers keep their own arms and match
// prefixes base SET does not have.
//
// VERIFIED before moving: no parser between GET's arm and SET's old position matches
// `*3\r\n$3\r\n`. Re-verify if arms are inserted into that span.
```

## The cost this trade actually pays, stated because it is not free

Every arm SET jumps over is now tried AFTER base SET, so packets destined for those arms
pay one additional failed header compare — SET's `*3\r\n$3\r\n` check, which fails on the
first byte pair for any other array length. That is a handful of instructions on SET's
option forms and on WATCH/UNWATCH/keyed_pop, against ~450 instr/op recovered on every
plain SET. SET is the most frequently issued command in any Redis workload; the trade is
strongly positive on aggregate but it is NOT a pure win, and the regression check below
exists because of it.

## Tests

**1. Behavioural — `scripts/dispatch_route_differ.py`.** The option forms are what an
ordering change can disturb, so they are the rows that matter:

```python
    # (frankenredis-z2ce3) Base SET's arm moved ahead of the option arms. These rows
    # exist to prove the option forms still reach THEIR OWN arms and are not
    # intercepted. A reply-only corpus would pass even if SET NX silently became
    # SET, so each write is followed by a read that distinguishes them.
    ("DEL", "s:mv"),
    ("SET", "s:mv", "first"),
    ("SET", "s:mv", "second", "NX"),        # must NOT overwrite
    ("GET", "s:mv"),                         # -> "first"
    ("SET", "s:mv", "third", "XX"),          # must overwrite
    ("GET", "s:mv"),                         # -> "third"
    ("SET", "s:mv", "fourth", "GET"),        # returns prior value
    ("GET", "s:mv"),
    ("SET", "s:ex", "v", "EX", "100"),
    ("TTL", "s:ex"),
    ("SET", "s:kt", "v"),
    ("SET", "s:kt", "w", "KEEPTTL"),
    ("GET", "s:kt"),
    ("SET", "s:plain", "v"),                 # the moved arm itself
    ("GET", "s:plain"),
```

**2. Ordering — assert the property, not the line number.** A test that pins arm order by
position rots on the next edit. What matters is that base SET is reached before the arms
it now precedes, which is observable only through cost, not behaviour — so there is NO
unit test for it. The ordering claim is carried by the measurement below and by the
comment at the site. Say so rather than write a test that appears to check it and does
not.

## Prediction, using this ledger's refined model

Landed rewires reach ~450-520 instr/op of dispatch rather than the reference route's 275,
and front-classification saves ~160 instr/op beyond the dispatch delta (argv
materialisation the share does not count):

    fr 2131.7 - (723 - 500) - 160  =  ~1,749 instr/op
    against redis 4,102.4          =  ~0.426x, from 0.5198x

The model is four-for-four within ~4 pct on rewires. NOTE THIS IS NOT A REWIRE — it is a
reordering, so the ~160 argv term may not apply; if it does not, expect ~1,909 and
~0.465x. **Both figures are on record before the measurement, which is the point.**

## After landing

Measure `set_base` AND `set_same` — this ledger established they agree to 0.26 pct on
dispatch, so both should move together, and a divergence would mean the change did
something shape-specific and unintended. Then `set_xx_opt` and `set_ex_opt` as the
regression check for the extra failed compare. Instruction counts only: the throughput
harness has been deferred five times on this host and its A/A null has never held.
