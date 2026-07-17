//! perf-stat / perf-record bench for cjson.decode — parsing JSON input, as common as encode in
//! real redis Lua scripts.
//!
//! Each eval decodes a ~46-entry JSON object (40-element number array + string + nested array) 200x,
//! so cjson.decode dominates over the one-time eval setup. Store reused, chunk cached. Measure:
//!
//!   perf stat -e instructions:u <bench-bin>
//!   perf record -g <bench-bin>

use std::hint::black_box;

use fr_command::lua_eval::eval_script;
use fr_store::Store;

const EVALS: usize = 4_000;
const DECODES_PER_EVAL: usize = 200;

fn main() {
    let mut store = Store::new();
    // A representative JSON payload: number array + a string field + a nested array.
    let script: &[u8] = b"local j='{\"1\":3,\"2\":6,\"3\":9,\"4\":12,\"5\":15,\"6\":18,\"7\":21,\"8\":24,\"9\":27,\"10\":30,\"name\":\"hello world\",\"nested\":[10,20,30,40,50]}' local t for k=1,200 do t=cjson.decode(j) end return type(t)";
    // Warm the compiled-chunk cache so the loop measures decoding, not compilation.
    let _ = eval_script(script, &[], &[], &mut store, 0);

    let mut acc = 0u64;
    for _ in 0..EVALS {
        let r = eval_script(black_box(script), &[], &[], &mut store, 0);
        acc = acc.wrapping_add(u64::from(r.is_ok()));
    }
    println!(
        "evals={EVALS} decodes={} ok_checksum={}",
        EVALS * DECODES_PER_EVAL,
        black_box(acc)
    );
}
