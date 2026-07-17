//! perf-stat / perf-record bench for cjson.encode — very common in real redis Lua scripts.
//!
//! Each eval builds a ~46-entry mixed table (40-element array + string key + nested array) and
//! encodes it 200x, so cjson.encode dominates over the one-time eval setup. Store is reused and the
//! compiled chunk is cached. Reports a checksum so the loop can't be elided. Measure:
//!
//!   perf stat -e instructions:u <bench-bin>
//!   perf record -g <bench-bin>

use std::hint::black_box;

use fr_command::lua_eval::eval_script;
use fr_store::Store;

const EVALS: usize = 4_000;
const ENCODES_PER_EVAL: usize = 200;

fn main() {
    let mut store = Store::new();
    let script: &[u8] = b"local t={} for i=1,40 do t[i]=i*3 end t.name='hello world' t.nested={10,20,30,40,50} local s for j=1,200 do s=cjson.encode(t) end return #s";
    // Warm the compiled-chunk cache so the loop measures encoding, not compilation.
    let _ = eval_script(script, &[], &[], &mut store, 0);

    let mut acc = 0u64;
    for _ in 0..EVALS {
        let r = eval_script(black_box(script), &[], &[], &mut store, 0);
        acc = acc.wrapping_add(u64::from(r.is_ok()));
    }
    println!(
        "evals={EVALS} encodes={} ok_checksum={}",
        EVALS * ENCODES_PER_EVAL,
        black_box(acc)
    );
}
