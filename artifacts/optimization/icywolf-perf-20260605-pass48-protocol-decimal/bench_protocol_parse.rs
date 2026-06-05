use std::env;
use std::time::Instant;

use fr_protocol::{parse_command_args_borrowed_into, BorrowedCommandArgsKind, ParserConfig};

fn push_bulk(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(b"$");
    out.extend_from_slice(bytes.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

fn build_hset_command(fields: usize, value_size: usize) -> Vec<u8> {
    let argc = 2 + fields * 2;
    let mut out = Vec::with_capacity(32 + fields * (value_size + 32));
    out.extend_from_slice(b"*");
    out.extend_from_slice(argc.to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    push_bulk(&mut out, b"HSET");
    push_bulk(&mut out, b"bench:key");
    for field in 0..fields {
        let field_name = format!("field:{field:04}");
        push_bulk(&mut out, field_name.as_bytes());
        let value = vec![b'x' + (field % 13) as u8; value_size];
        push_bulk(&mut out, &value);
    }
    out
}

fn arg_usize(args: &[String], idx: usize, default: usize) -> usize {
    args.get(idx)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let iterations = arg_usize(&args, 1, 2_000_000);
    let fields = arg_usize(&args, 2, 16);
    let value_size = arg_usize(&args, 3, 64);
    let packet = build_hset_command(fields, value_size);
    let cfg = ParserConfig::default();
    let mut argv = Vec::with_capacity(2 + fields * 2);
    let mut checksum = 0usize;

    let start = Instant::now();
    for _ in 0..iterations {
        let parsed =
            parse_command_args_borrowed_into(&packet, &cfg, &mut argv).expect("packet parses");
        assert_eq!(parsed.kind, BorrowedCommandArgsKind::Arguments);
        checksum = checksum
            .wrapping_add(parsed.consumed)
            .wrapping_add(argv.len())
            .wrapping_add(argv[0].len())
            .wrapping_add(argv.last().map_or(0, |arg| arg.len()));
    }
    let elapsed = start.elapsed();
    let elapsed_ns = elapsed.as_nanos();
    let ns_per_iter = elapsed_ns as f64 / iterations as f64;
    println!(
        "iterations={iterations} fields={fields} value_size={value_size} packet_bytes={} checksum={checksum} elapsed_ns={elapsed_ns} ns_per_iter={ns_per_iter:.3}",
        packet.len()
    );
}
