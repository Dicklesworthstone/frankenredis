use criterion::{Criterion, criterion_group, criterion_main};
use fr_command::lua_eval::{eval_script, eval_script_cloned_globals_for_bench};
use fr_store::Store;

fn bench_lua_eval(c: &mut Criterion) {
    const RETURN_ONE: &[u8] = b"return 1";
    const NUMERIC_FOR_SUM: &[u8] = b"local s=0; for i=1,1000 do s=s+i end; return s";
    const NUMERIC_FOR_SUM_SQUARES: &[u8] = b"local s=0; for i=1,1000 do s=s+i*i end; return s";

    let mut group = c.benchmark_group("lua_eval");
    group.bench_function("return_one_orig_cloned_globals", |b| {
        let mut store = Store::new();
        b.iter(|| {
            std::hint::black_box(eval_script_cloned_globals_for_bench(
                std::hint::black_box(RETURN_ONE),
                &[],
                &[],
                &mut store,
                1_000,
            ))
            .unwrap()
        })
    });
    group.bench_function("return_one_overlay_globals", |b| {
        let mut store = Store::new();
        b.iter(|| {
            std::hint::black_box(eval_script(
                std::hint::black_box(RETURN_ONE),
                &[],
                &[],
                &mut store,
                1_000,
            ))
            .unwrap()
        })
    });
    group.bench_function("numeric_for_sum_1000", |b| {
        // Store is created ONCE (matching the return_one benches above); the prior code did
        // `Store::new()` per iteration, so this benchmark measured Store::default's two
        // generate_run_id() calls (getpid + SystemTime + hex format!) instead of the interpreter.
        let mut store = Store::new();
        b.iter(|| {
            std::hint::black_box(eval_script(
                std::hint::black_box(NUMERIC_FOR_SUM),
                &[],
                &[],
                &mut store,
                1_000,
            ))
            .unwrap()
        })
    });
    group.bench_function("numeric_for_sum_squares_1000", |b| {
        let mut store = Store::new();
        b.iter(|| {
            std::hint::black_box(eval_script(
                std::hint::black_box(NUMERIC_FOR_SUM_SQUARES),
                &[],
                &[],
                &mut store,
                1_000,
            ))
            .unwrap()
        })
    });
    group.finish();
}

criterion_group!(benches, bench_lua_eval);
criterion_main!(benches);
