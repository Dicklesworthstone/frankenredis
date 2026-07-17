//! perf-stat / perf-record bench for the Lua string pattern matcher (string.gsub/match/find) —
//! a hand-rolled matcher common in real redis Lua scripts (key parsing, validation, sanitizing).
//!
//! Each eval runs a gsub (replace all matches) and several match/find calls over a moderate string
//! 100x, so the matcher dominates over the one-time eval setup. Store reused, chunk cached. Measure:
//!
//!   perf stat -e instructions:u <bench-bin>
//!   perf record -g <bench-bin>

use std::hint::black_box;

use fr_command::lua_eval::eval_script;
use fr_store::Store;

const EVALS: usize = 20_000;

fn main() {
    let mut store = Store::new();
    // A non-anchored capturing pattern search over a moderate string: exercises the start-position
    // loop (many failed positions before a match) plus captures.
    let script: &[u8] = b"local s='user:12345:session:abcdef host:node-42 ttl:3600 flags:RW payload:hello_world_data' local n=0 for i=1,100 do local a,b=string.match(s,'(%w+):(%w+)') local c=string.gsub(s,'(%w+):(%w+)','%2=%1') local d=string.find(s,'ttl:(%d+)') n=n+#c end return n";
    // Warm the compiled-chunk cache so the loop measures matching, not compilation.
    let _ = eval_script(script, &[], &[], &mut store, 0);

    let mut acc = 0u64;
    for _ in 0..EVALS {
        let r = eval_script(black_box(script), &[], &[], &mut store, 0);
        acc = acc.wrapping_add(u64::from(r.is_ok()));
    }
    println!("evals={EVALS} ok_checksum={}", black_box(acc));
}
