//! perf-stat instructions:u bench for the per-EVAL coroutine-table `format!` elimination.
//!
//! `LuaState::set_keys_argv` runs once per EVAL and rebuilds the sandbox `coroutine` table by doing
//! `format!("coroutine.{name}")` for all six methods — six `String` allocations + Display formatting
//! per script execution, for constant names. This bench evals a trivial script (`return 1`) in a
//! tight loop with a REUSED store (so Store::new()/generate_run_id is out of the measurement) and
//! reports a checksum so the loop can't be elided. Measure with:
//!
//!   perf stat -e instructions:u <bench-bin>
//!
//! Compare the count before vs after the fix (static full-name literals). Behaviour is byte-identical
//! (same coroutine method names), asserted by the fr-command lua tests.

use std::hint::black_box;

use fr_command::lua_eval::eval_script;
use fr_store::Store;

const EVALS: usize = 200_000;

fn main() {
    let mut store = Store::new();
    let script: &[u8] = b"return 1";
    // Warm the compiled-chunk cache so the loop measures eval, not compilation.
    let _ = eval_script(script, &[], &[], &mut store, 0);

    let mut acc = 0u64;
    for _ in 0..EVALS {
        let r = eval_script(black_box(script), &[], &[], &mut store, 0);
        acc = acc.wrapping_add(u64::from(r.is_ok()));
    }
    println!("evals={EVALS} ok_checksum={}", black_box(acc));
}
