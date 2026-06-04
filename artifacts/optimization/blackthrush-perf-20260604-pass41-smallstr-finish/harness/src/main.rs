use std::hint::black_box;
use std::time::Instant;

use fr_store::Store;

const DEFAULT_KEYS: usize = 300_000;
const NOW_MS: u64 = 1_778_889_600_000;

fn main() {
    let keys = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_KEYS);

    let mut store = Store::new();
    let start = Instant::now();
    for i in 0..keys {
        let key = format!("sso:{i:08}").into_bytes();
        store.set(key, b"abc".to_vec(), None, NOW_MS);
    }
    let set_elapsed = start.elapsed();

    let mut checksum = 0usize;
    let get_start = Instant::now();
    for i in (0..keys).step_by(97) {
        let key = format!("sso:{i:08}");
        if let Some(value) = store.get(key.as_bytes(), NOW_MS).expect("string get") {
            checksum = checksum.wrapping_mul(131).wrapping_add(value.len());
        }
    }
    let get_elapsed = get_start.elapsed();

    let logical_memory = store.estimate_memory_usage_bytes();
    println!("keys={keys}");
    println!("set_ns={}", set_elapsed.as_nanos());
    println!("get_ns={}", get_elapsed.as_nanos());
    println!("logical_memory={logical_memory}");
    println!("checksum={}", black_box(checksum));
}
