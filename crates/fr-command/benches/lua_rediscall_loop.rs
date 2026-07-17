//! perf-stat / perf-record bench for the `redis.call` in-loop marshalling path — the hot path of
//! real redis Lua scripts (rate limiters, atomic get-modify-set, etc.).
//!
//! Each eval runs `for i=1,50 do redis.call('GET', KEYS[1]) end` — 50 redis.call round trips
//! (Lua arg eval → build argv Vec<Vec<u8>> → dispatch_argv(GET) → RespFrame → resp_to_lua) that
//! dominate over the one-time per-eval setup. Store is reused (created once) and the target key is
//! pre-seeded. Reports a checksum so the loop can't be elided. Measure:
//!
//!   perf stat -e instructions:u <bench-bin>
//!   perf record -g <bench-bin>

use std::hint::black_box;

use fr_command::dispatch_argv;
use fr_command::lua_eval::eval_script;
use fr_store::Store;

const EVALS: usize = 20_000;
const CALLS_PER_EVAL: usize = 50;

fn main() {
    let mut store = Store::new();
    // Seed the key the script GETs so every redis.call hits (a bulk-string reply to convert).
    let _ = dispatch_argv(
        &[b"SET".to_vec(), b"k".to_vec(), b"val".to_vec()],
        &mut store,
        0,
    );

    let script: &[u8] = b"for i=1,50 do redis.call('GET', KEYS[1]) end return 1";
    let keys = [b"k".to_vec()];
    // Warm the compiled-chunk cache so the loop measures execution, not compilation.
    let _ = eval_script(script, &keys, &[], &mut store, 0);

    let mut acc = 0u64;
    for _ in 0..EVALS {
        let r = eval_script(black_box(script), &keys, &[], &mut store, 0);
        acc = acc.wrapping_add(u64::from(r.is_ok()));
    }
    println!(
        "evals={EVALS} redis_calls={} ok_checksum={}",
        EVALS * CALLS_PER_EVAL,
        black_box(acc)
    );
}
