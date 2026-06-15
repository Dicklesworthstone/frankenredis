use fr_store::Store;
use std::env;
use std::error::Error;
use std::fs;
use std::hint::black_box;
use std::io;
use std::time::Instant;

const NOW_MS: u64 = 1_777_100_000_000;
type HarnessResult<T> = Result<T, Box<dyn Error>>;

fn mix(mut acc: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        acc ^= u64::from(byte);
        acc = acc.wrapping_mul(0x100_0000_01b3);
        acc = acc.rotate_left(5);
    }
    acc
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    for pair in args.windows(2) {
        if let [flag, value] = pair
            && flag == name
        {
            return Some(value.clone());
        }
    }
    None
}

fn arg_usize(args: &[String], name: &str, default: usize) -> usize {
    arg_value(args, name)
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(default)
}

fn value_for(index: usize, payload_size: usize) -> Vec<u8> {
    let mut value = format!("v:{index:08}:").into_bytes();
    if value.len() < payload_size {
        let pad = b'a' + u8::try_from(index % 26).unwrap_or(0);
        value.resize(payload_size, pad);
    } else {
        value.truncate(payload_size);
    }
    value
}

fn build_payload(list_len: usize, payload_size: usize) -> HarnessResult<Vec<u8>> {
    let mut store = Store::new();
    let values: Vec<Vec<u8>> = (0..list_len).map(|i| value_for(i, payload_size)).collect();
    for chunk in values.chunks(512) {
        store
            .rpush(b"source", chunk, NOW_MS)
            .map_err(|err| io::Error::other(format!("rpush source list failed: {err:?}")))?;
    }
    Ok(store
        .dump_key(b"source", NOW_MS)
        .ok_or_else(|| io::Error::other("source list must dump"))?)
}

fn restore_payload(payload: &[u8]) -> HarnessResult<Store> {
    let mut store = Store::new();
    store
        .restore_key(b"restored", 0, payload, true, NOW_MS)
        .map_err(|err| io::Error::other(format!("restore failed: {err:?}")))?;
    Ok(store)
}

fn write_golden_files(prefix: &str, payload: &[u8]) -> HarnessResult<()> {
    fs::write(format!("{prefix}-source.dump"), payload)?;

    let mut store = restore_payload(payload)?;
    let restored = store
        .dump_key(b"restored", NOW_MS)
        .ok_or_else(|| io::Error::other("restored list must dump"))?;
    fs::write(format!("{prefix}-restored.dump"), &restored)?;

    let head = store
        .lrange(b"restored", 0, 2, NOW_MS)
        .map_err(|err| io::Error::other(format!("lrange failed: {err:?}")))?;
    let len = store
        .llen(b"restored", NOW_MS)
        .map_err(|err| io::Error::other(format!("llen failed: {err:?}")))?;
    let encoding = store
        .object_encoding(b"restored", NOW_MS)
        .ok_or_else(|| io::Error::other("OBJECT ENCODING must exist"))?;

    println!("golden_source_len={}", payload.len());
    println!("golden_restored_len={}", restored.len());
    println!("golden_dumps_equal={}", payload == restored.as_slice());
    println!("golden_llen={len}");
    println!("golden_encoding={encoding}");
    println!("golden_head_len={}", head.len());
    Ok(())
}

fn dump_loop(payload: &[u8], repeats: usize) -> HarnessResult<(u128, u64, String, usize)> {
    let mut store = restore_payload(payload)?;
    let mut checksum = 0xcbf2_9ce4_8422_2325;

    let start = Instant::now();
    for _ in 0..repeats {
        let dump = store
            .dump_key(b"restored", NOW_MS)
            .ok_or_else(|| io::Error::other("restored list must dump in loop"))?;
        checksum = mix(checksum, &dump);
        black_box(dump);
    }
    let dump_ns = start.elapsed().as_nanos();
    let digest = store.state_digest();
    let memory_bytes = store.estimate_memory_usage_bytes();
    black_box(store);
    Ok((dump_ns, checksum, digest, memory_bytes))
}

fn main() -> HarnessResult<()> {
    let args: Vec<String> = env::args().collect();
    let list_len = arg_usize(&args, "--list-len", 10_000);
    let payload_size = arg_usize(&args, "--payload-size", 16);
    let repeats = arg_usize(&args, "--repeats", 2_000).max(1);
    let golden_prefix = arg_value(&args, "--golden-prefix");

    let payload = build_payload(list_len, payload_size)?;
    if let Some(prefix) = golden_prefix {
        write_golden_files(&prefix, &payload)?;
    }

    let (dump_ns, checksum, state_digest, memory_bytes) = dump_loop(&payload, repeats)?;
    println!("list_len={list_len}");
    println!("payload_size={payload_size}");
    println!("repeats={repeats}");
    println!("payload_bytes={}", payload.len());
    println!("dump_ns={dump_ns}");
    println!("dump_ns_per_op={:.3}", dump_ns as f64 / repeats as f64);
    println!("checksum={checksum}");
    println!("state_digest={state_digest}");
    println!("memory_bytes={memory_bytes}");
    Ok(())
}
