# z2ce3 — stop the borrowed fast paths un-borrowing their keys

SPEC + CODE, deliberately NOT in the tree. Build freeze at the 42G floor, and these regions have
collided repeatedly today. **Re-locate every site by CONTENT, never by line offset.** Verified
against HEAD; nothing compiled — expect one rustc error on first build.

## Both design questions are now RESOLVED, and both favour the fix

**1. The metrics closure is lazy.** `record_plain_zremrange_borrowed_metrics` takes
`build_argv: impl Fn() -> Vec<Vec<u8>>` and drives it through
`argv.get_or_insert_with(&build_argv)` on an `Option`, so it runs only if slowlog or latency
sampling fires, and at most once. Not a third copy.

**2. `last_del_removed` already clones conditionally.** `Store::del` gates on
`record_removed = self.notify_keyspace_events != 0`, and the existing comment says the unconditional
form was *"a wasted per-key heap allocation"* with notifications disabled — the default. So the
owned-key requirement is already scoped to the notify path.

**Therefore the unconditional `keys_owned` copy at the top of the executor is the only one left, and
removing it is mechanical.** That is what makes this a low-risk change rather than a redesign.

## fr-store — make the key representation generic, don't add a second function

Locate by content: `pub fn del(&mut self, keys: &[Vec<u8>], now_ms: u64) -> u64 {`

```rust
// (frankenredis-z2ce3) Generic over the key representation so the BORROWED executors
// can pass `&[&[u8]]` straight through instead of materialising an owned Vec first.
// The body never needed ownership — it used `drop_if_expired(key, ..)` and
// `internal_entries_remove(key.as_slice())`, both slice-only. `Vec<u8>: AsRef<[u8]>`
// so every existing owned caller compiles unchanged, and there is ONE function
// rather than two that can drift.
pub fn del<K: AsRef<[u8]>>(&mut self, keys: &[K], now_ms: u64) -> u64 {
    let mut removed = 0_u64;
    self.last_del_removed.clear();
    let record_removed = self.notify_keyspace_events != 0;
    for key in keys {
        let key = key.as_ref();                     // <- the one added line in the loop
        if self.expires_count != 0 {
            self.drop_if_expired(key, now_ms);
        }
        if self.internal_entries_remove(key).is_some() {   // was key.as_slice()
            removed += 1;
            if record_removed {
                self.last_del_removed.push(key.to_vec());  // owned ONLY when notifying
            }
        }
    }
    removed
}
```

The `key.to_vec()` inside `record_removed` is where ownership genuinely belongs: it is the one place
the owned bytes are actually kept.

## fr-runtime — pass the borrow through

Locate by content: `let keys_owned: Vec<Vec<u8>> = keys.iter().map(|k| k.to_vec()).collect();`
inside `pub fn execute_plain_del_borrowed`.

```rust
        // (frankenredis-z2ce3) `keys` is already &[&[u8]] — the whole point of this
        // route. It used to be copied into an owned Vec purely to satisfy `del`'s
        // signature; `del` is now generic and takes the borrow directly.
        let count = self.server.store.del(keys, now_ms);
        let _ = self.server.store.take_last_del_removed();
```

and the metrics closure, which must no longer reference `keys_owned`:

```rust
            || {
                let mut argv = Vec::with_capacity(keys.len() + 1);
                argv.push(b"DEL".to_vec());
                argv.extend(keys.iter().map(|k| k.to_vec()));
                argv
            },
```

This closure is lazy, so the `to_vec()`s here run only when slowlog or latency sampling fires —
which is the correct place for them and is unchanged in behaviour.

`execute_plain_unlink_borrowed` is the identical edit with `b"UNLINK"`.

## The other five, NOT done here

`zinter`, `zstore`, `zdiffstore`, `exists_multi`, `touch` do the same `keys.iter().map(|k|
k.to_vec()).collect()`. Each needs the SAME check before conversion, not an assumption:

1. does the store fn it calls use its keys only as slices?
2. does anything downstream retain owned keys unconditionally?

DEL and UNLINK are done first precisely because both answers were verified, and `del_1_missing` is
the measured worst shape so it is where the number will show.

## Tests

Locate by content: the `fn del` tests in fr-store. Add both call styles so the generic signature is
exercised in both directions and cannot regress to owned-only:

```rust
#[test]
fn del_accepts_both_owned_and_borrowed_keys() {
    // (frankenredis-z2ce3) The generic signature is the lever. If someone
    // "simplifies" it back to &[Vec<u8>], the borrowed executors silently regain
    // their per-key allocation and NOTHING ELSE FAILS — no reply changes, no test
    // reddens. This is the assertion that would.
    let mut store = Store::default();
    store.set_string(b"a".to_vec(), b"1".to_vec(), 0);
    store.set_string(b"b".to_vec(), b"1".to_vec(), 0);

    let owned: Vec<Vec<u8>> = vec![b"a".to_vec()];
    assert_eq!(store.del(&owned, 0), 1, "owned call style must still work");

    let borrowed: [&[u8]; 1] = [b"b"];
    assert_eq!(store.del(&borrowed, 0), 1, "borrowed call style must work");

    assert_eq!(store.del(&borrowed, 0), 0, "second delete of a gone key removes nothing");
}
```

**The comment on that test is the point of it.** This lever has no behavioural signature: reverting
it changes no reply and reddens no existing test, so without an explicit assertion on the *call
style* it would silently rot. Same reasoning as the pair-level invariant test — the property worth
pinning is the one no functional test can see.

## Before landing

MEASURE `del_1_missing` FIRST. The bead does not size this, and with mimalloc two small allocations
on a ~1500 instr/op operation may be a few percent rather than a multiple. If it is a few percent,
say so — a small honest number is worth more than an inflated one, and this row would be the first
allocation-side result in a campaign that has been entirely dispatch-side.
