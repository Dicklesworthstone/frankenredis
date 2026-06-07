use std::env;
use std::hint::black_box;
use std::time::Instant;

use fr_store::{Store, encode_db_key};

#[derive(Clone, Copy)]
enum Mode {
    RandomkeyInDb,
    KeysInDb,
    GlobalRandomkey,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "randomkey-in-db" => Ok(Self::RandomkeyInDb),
            "keys-in-db" => Ok(Self::KeysInDb),
            "global-randomkey" => Ok(Self::GlobalRandomkey),
            _ => Err(format!(
                "unsupported --mode {value}; use randomkey-in-db|keys-in-db|global-randomkey"
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::RandomkeyInDb => "randomkey-in-db",
            Self::KeysInDb => "keys-in-db",
            Self::GlobalRandomkey => "global-randomkey",
        }
    }
}

struct Config {
    mode: Mode,
    keys: usize,
    iterations: usize,
    db: usize,
    value_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::RandomkeyInDb,
            keys: 100_000,
            iterations: 1_000,
            db: 0,
            value_size: 3,
        }
    }
}

fn main() {
    let config = match parse_args() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };

    let prefill_start = Instant::now();
    let mut store = Store::new();
    let value = vec![b'x'; config.value_size];
    for index in 0..config.keys {
        let logical = format!("rk:{index:012}").into_bytes();
        store.set(encode_db_key(config.db, &logical), value.clone(), None, 0);
    }
    let prefill_ms = prefill_start.elapsed().as_secs_f64() * 1_000.0;

    let timed_start = Instant::now();
    let mut checksum = 0usize;
    match config.mode {
        Mode::RandomkeyInDb => {
            for _ in 0..config.iterations {
                black_box(&mut store);
                if let Some(key) = store.randomkey_in_db(black_box(config.db), black_box(0)) {
                    checksum = checksum.wrapping_add(checksum_key(&key));
                    black_box(key);
                }
            }
        }
        Mode::KeysInDb => {
            for _ in 0..config.iterations {
                black_box(&mut store);
                let keys = store.keys_in_db(black_box(config.db), black_box(0));
                checksum = checksum.wrapping_add(keys.iter().map(|key| key.len()).sum::<usize>());
                black_box(keys);
            }
        }
        Mode::GlobalRandomkey => {
            for _ in 0..config.iterations {
                black_box(&mut store);
                if let Some(key) = store.randomkey(black_box(0)) {
                    checksum = checksum.wrapping_add(checksum_key(&key));
                    black_box(key);
                }
            }
        }
    }
    let elapsed = timed_start.elapsed();
    let elapsed_secs = elapsed.as_secs_f64();
    let ops_per_sec = config.iterations as f64 / elapsed_secs;
    let ns_per_op = elapsed.as_nanos() as f64 / config.iterations as f64;

    println!(
        "{{\"mode\":\"{}\",\"keys\":{},\"iterations\":{},\"db\":{},\"value_size\":{},\"prefill_ms\":{:.3},\"elapsed_ms\":{:.3},\"ops_per_sec\":{:.3},\"ns_per_op\":{:.1},\"checksum\":{}}}",
        config.mode.as_str(),
        config.keys,
        config.iterations,
        config.db,
        config.value_size,
        prefill_ms,
        elapsed_secs * 1_000.0,
        ops_per_sec,
        ns_per_op,
        checksum
    );
}

fn parse_args() -> Result<Config, String> {
    let mut config = Config::default();
    let mut args = env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--mode" => {
                config.mode = Mode::parse(&next_value(&mut args, &flag)?)?;
            }
            "--keys" => {
                config.keys = parse_usize(&next_value(&mut args, &flag)?, &flag)?;
            }
            "--iterations" => {
                config.iterations = parse_usize(&next_value(&mut args, &flag)?, &flag)?;
            }
            "--db" => {
                config.db = parse_usize(&next_value(&mut args, &flag)?, &flag)?;
            }
            "--value-size" => {
                config.value_size = parse_usize(&next_value(&mut args, &flag)?, &flag)?;
            }
            "--help" | "-h" => return Err(help_text()),
            _ => return Err(format!("unknown flag {flag}\n{}", help_text())),
        }
    }
    if config.keys == 0 {
        return Err("--keys must be greater than zero".to_string());
    }
    if config.iterations == 0 {
        return Err("--iterations must be greater than zero".to_string());
    }
    Ok(config)
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn parse_usize(value: &str, flag: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|err| format!("invalid {flag} value {value}: {err}"))
}

fn checksum_key(key: &[u8]) -> usize {
    key.iter().fold(0usize, |acc, byte| {
        acc.wrapping_mul(16_777_619).wrapping_add(*byte as usize)
    })
}

fn help_text() -> String {
    "usage: fr-srhju-randomkey-bench [--mode randomkey-in-db|keys-in-db|global-randomkey] [--keys N] [--iterations N] [--db N] [--value-size N]".to_string()
}
