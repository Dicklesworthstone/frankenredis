use criterion::{Criterion, criterion_group, criterion_main};
use fr_command::lua_eval::eval_script;
use fr_store::Store;

fn bench_lua_eval(c: &mut Criterion) {
    const NUMERIC_FOR_SUM: &[u8] = b"local s=0; for i=1,1000 do s=s+i end; return s";
    const NUMERIC_FOR_SUM_SQUARES: &[u8] =
        b"local s=0; for i=1,1000 do s=s+i*i end; return s";

    let mut group = c.benchmark_group("lua_eval");
    group.bench_function("numeric_for_sum_1000", |b| {
        b.iter(|| {
            let mut store = Store::new();
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
        b.iter(|| {
            let mut store = Store::new();
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
