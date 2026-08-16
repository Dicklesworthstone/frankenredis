# z2ce3 — the five closure-only routes. No store change, no signature change.

SPEC + CODE, deliberately not in the tree. Build freeze at the 42G floor; these regions have collided
repeatedly. **Re-locate every site by CONTENT.** Nothing compiled.

Land these BEFORE the DEL/UNLINK half: they are local to one closure per function, touch no store
signature, and cross no crate boundary.

## The transformation, identical in all five

Delete the unconditional copy:

```rust
let keys_owned: Vec<Vec<u8>> = keys.iter().map(|k| k.to_vec()).collect();
```

and have the closure build its owned argv from the borrow instead. The closure is already lazy
(`argv.get_or_insert_with(&build_argv)`), so the allocations move *inside the branch that wants
them* and disappear from the default path entirely.

### `execute_plain_touch_borrowed` (~27122) — cleanest, `Store::touch` already takes `&[&[u8]]`

```rust
            || {
                let mut argv = Vec::with_capacity(keys.len() + 1);
                argv.push(b"TOUCH".to_vec());
                argv.extend(keys.iter().map(|k| k.to_vec()));
                argv
            },
```

### `execute_plain_exists_multi_borrowed` (~27077) — same, `b"EXISTS"`

The work is already done entirely on borrows above (`for &k in keys { store.exists_no_touch(k, ..) }`)
before the copy is even taken, so nothing else in the function references `keys_owned`.

### `execute_plain_zinter_borrowed` (~25208)

```rust
            || {
                let mut argv = Vec::with_capacity(keys.len() + 2);
                argv.push(b"ZINTER".to_vec());
                argv.push(numkeys_arg.to_vec());
                argv.extend(keys.iter().map(|k| k.to_vec()));
                argv
            },
```

### `execute_plain_zstore_borrowed` (~26836)

```rust
            || {
                let mut argv = Vec::with_capacity(keys.len() + 3);
                argv.push(name_upper.as_bytes().to_vec());
                argv.push(dest_owned.clone());
                argv.push(numkeys_arg.to_vec());
                argv.extend(keys.iter().map(|k| k.to_vec()));
                argv
            },
```

### `execute_plain_zdiffstore_borrowed` (~26951) — same with `b"ZDIFFSTORE"`

## A SECOND allocation in three of them, which I have NOT verified

`zinter`, `zstore` and `zdiffstore` also take `let numkeys_owned = numkeys_arg.to_vec();` (spelled
`nk_owned` in two of them) unconditionally, and the code above folds it into the closure as
`numkeys_arg.to_vec()`.

**That is only correct if `numkeys_owned`/`nk_owned` has no other consumer in the function.** I did
not check. Verify before applying — if it is used elsewhere, keep the binding and leave
`.clone()` in the closure as it is today.

`dest_owned` is deliberately left alone in both store variants: a destination key plausibly IS needed
by the store call, and I have not read those paths. Do not fold it in on the strength of this doc.

## Why the closure capture is safe

The closure is `impl Fn() -> Vec<Vec<u8>>` and is consumed within the same call, so capturing `keys:
&[&[u8]]` by reference outlives nothing. It is already capturing `keys_owned` by reference in exactly
the same position; the borrow it takes is no longer-lived than the one it replaces.

## Tests

Extend `scratchpad/del_borrowed_allocation_census.rs` rather than writing new files — one census
covering the family reads better than five, and BlackCat's point applies: pin the allocation COUNT,
not the call style.

`touch_missing` and `exists_missing` are ALREADY harness shapes, which makes those two the cheapest
rows to add and the right ones to land first:

```rust
#[test]
fn borrowed_touch_and_exists_allocate_nothing_per_key_by_default() {
    // (frankenredis-z2ce3) Both routes do their entire job on borrows and then
    // allocated an owned copy whose ONLY consumer is the lazy metrics closure --
    // which, with slowlog and latency sampling off (the default), never runs. So
    // the default path allocated a Vec plus one Vec<u8> per key to build an argv
    // that was then never built.
    //
    // This asserts the property, not the shape of the fix: any reintroduction of
    // an unconditional copy reddens it, wherever it is written.
    let mut rt = fr_runtime::Runtime::default_strict();
    let keys: [&[u8]; 2] = [b"nosuch:a", b"nosuch:b"];

    let mut probe = |n: usize| -> usize {
        let before = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..n {
            assert_eq!(
                rt.execute_plain_touch_borrowed(&keys, 1),
                Some(fr_protocol::RespFrame::Integer(0)),
                "borrowed TOUCH must serve missing keys"
            );
        }
        ALLOCS.load(Ordering::Relaxed) - before
    };
    let _ = probe(64);
    let few = probe(128);
    let many = probe(384);
    let per_op = many.saturating_sub(few) as f64 / 256.0;

    eprintln!("borrowed TOUCH (2 missing keys): {per_op:.3} allocations/op");
    // Before: >= 3 per op (the Vec plus one to_vec() per key, times two keys).
    // After: 0. Bound sits between so neither state passes in the other's place.
    assert!(
        per_op < 1.0,
        "borrowed TOUCH still allocates per call: {per_op:.3}/op — the metrics \
         closure is lazy, so nothing on the default path should allocate here"
    );
}
```

The `assert_eq!` inside the loop is load-bearing for the same reason as in the DEL census: a `None`
means the route declined its gate and the census would count nothing while passing.

## Before landing

**Not sized.** All of this is structural. The census pins the invariant; a callgrind row on
`touch_missing` / `exists_missing` sizes it. Keep those two questions apart — a small cost number
must not be read as "the invariant does not matter", and on these five the *count* is unambiguous
even if the cost turns out small.
