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

fn mix_u64(acc: u64, value: u64) -> u64 {
    mix(acc, &value.to_le_bytes())
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

fn arg_path(args: &[String], name: &str) -> Option<String> {
    arg_value(args, name)
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

fn write_golden_files(prefix: &str, payload: &[u8]) -> HarnessResult<()> {
    let source_path = format!("{prefix}-source.dump");
    fs::write(&source_path, payload)?;

    let mut store = Store::new();
    store
        .restore_key(b"restored", 0, payload, true, NOW_MS)
        .map_err(|err| io::Error::other(format!("golden restore failed: {err:?}")))?;
    let restored = store
        .dump_key(b"restored", NOW_MS)
        .ok_or_else(|| io::Error::other("restored list must dump"))?;
    let restored_path = format!("{prefix}-restored.dump");
    fs::write(&restored_path, &restored)?;

    let head = store
        .lrange(b"restored", 0, 2, NOW_MS)
        .map_err(|err| io::Error::other(format!("golden lrange failed: {err:?}")))?;
    let len = store
        .llen(b"restored", NOW_MS)
        .map_err(|err| io::Error::other(format!("golden llen failed: {err:?}")))?;
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

fn restore_loop(
    payload: &[u8],
    repeats: usize,
    ring: usize,
) -> HarnessResult<(u128, u64, String, usize)> {
    let mut store = Store::new();
    let mut checksum = 0xcbf2_9ce4_8422_2325;
    let keys: Vec<Vec<u8>> = (0..ring)
        .map(|i| format!("dst:{i:04}").into_bytes())
        .collect();

    let start = Instant::now();
    for i in 0..repeats {
        let key = keys
            .get(i % keys.len())
            .ok_or_else(|| io::Error::other("ring key missing"))?;
        store
            .restore_key(key, 0, payload, true, NOW_MS)
            .map_err(|err| io::Error::other(format!("restore loop failed: {err:?}")))?;
        if i % 17 == 0 {
            let len = store
                .llen(key, NOW_MS)
                .map_err(|err| io::Error::other(format!("restore loop llen failed: {err:?}")))?;
            checksum = mix_u64(checksum, len as u64);
            if let Some(dump) = store.dump_key(key, NOW_MS) {
                checksum = mix_u64(checksum, dump.len() as u64);
                if let Some(head) = dump.get(..dump.len().min(64)) {
                    checksum = mix(checksum, head);
                }
            }
        }
    }
    let restore_ns = start.elapsed().as_nanos();
    let digest = store.state_digest();
    let memory_bytes = store.estimate_memory_usage_bytes();
    black_box(store);
    Ok((restore_ns, checksum, digest, memory_bytes))
}

fn main() -> HarnessResult<()> {
    let args: Vec<String> = env::args().collect();
    let list_len = arg_usize(&args, "--list-len", 10_000);
    let payload_size = arg_usize(&args, "--payload-size", 16);
    let repeats = arg_usize(&args, "--repeats", 200).max(1);
    let ring = arg_usize(&args, "--ring", 8).max(1);
    let golden_prefix = arg_path(&args, "--golden-prefix");

    let payload = build_payload(list_len, payload_size)?;
    if let Some(prefix) = golden_prefix {
        write_golden_files(&prefix, &payload)?;
    }

    let (restore_ns, checksum, state_digest, memory_bytes) = restore_loop(&payload, repeats, ring)?;
    println!("list_len={list_len}");
    println!("payload_size={payload_size}");
    println!("repeats={repeats}");
    println!("ring={ring}");
    println!("payload_bytes={}", payload.len());
    println!("restore_ns={restore_ns}");
    println!(
        "restore_ns_per_op={:.3}",
        restore_ns as f64 / repeats as f64
    );
    println!("checksum={checksum}");
    println!("state_digest={state_digest}");
    println!("memory_bytes={memory_bytes}");
    Ok(())
}
