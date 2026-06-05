use std::hint::black_box;
use std::io::{self, Write};
use std::time::Instant;

use fr_protocol::{
    BorrowedCommandFrame, RespFrame, parse_command_frame, parse_command_frame_borrowed,
};

fn push_command(out: &mut Vec<u8>, parts: &[&[u8]]) {
    out.extend_from_slice(b"*");
    out.extend_from_slice(parts.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for part in parts {
        out.extend_from_slice(b"$");
        out.extend_from_slice(part.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(part);
        out.extend_from_slice(b"\r\n");
    }
}

fn request_frame(field_count: usize, value_size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let value = vec![b'x'; value_size];
    for i in 0..field_count {
        let field = format!("field:{i}");
        push_command(&mut out, &[b"HSET", b"hot:hash", field.as_bytes(), &value]);
    }
    out
}

fn write_bulk(out: &mut Vec<u8>, arg: &[u8]) {
    out.extend_from_slice(arg.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(arg);
    out.push(b'\n');
}

fn write_owned_canonical(out: &mut Vec<u8>, frame: &RespFrame) -> Result<(), String> {
    match frame {
        RespFrame::Array(Some(items)) => {
            out.extend_from_slice(items.len().to_string().as_bytes());
            out.push(b'\n');
            for item in items {
                if let RespFrame::BulkString(Some(arg)) = item {
                    write_bulk(out, arg);
                } else {
                    return Err(format!(
                        "command frame contained non-bulk argument: {item:?}"
                    ));
                }
            }
            Ok(())
        }
        RespFrame::Array(None) => {
            out.extend_from_slice(b"null\n");
            Ok(())
        }
        other => Err(format!("unexpected owned command frame: {other:?}")),
    }
}

fn write_borrowed_canonical(
    out: &mut Vec<u8>,
    frame: &BorrowedCommandFrame<'_>,
) -> Result<(), String> {
    match frame {
        BorrowedCommandFrame::Arguments(args) => {
            out.extend_from_slice(args.len().to_string().as_bytes());
            out.push(b'\n');
            for arg in args {
                write_bulk(out, arg);
            }
            Ok(())
        }
        BorrowedCommandFrame::NullArray => {
            out.extend_from_slice(b"null\n");
            Ok(())
        }
        BorrowedCommandFrame::Owned(other) => Err(format!("unexpected owned fallback: {other:?}")),
    }
}

fn write_golden(input: &[u8], borrowed: bool) -> Result<(), String> {
    let mut cursor = 0;
    let mut out = Vec::new();
    while cursor < input.len() {
        if borrowed {
            let parsed = parse_command_frame_borrowed(&input[cursor..], &Default::default())
                .map_err(|err| format!("borrowed frame parse failed: {err}"))?;
            write_borrowed_canonical(&mut out, &parsed.frame)?;
            cursor += parsed.consumed;
        } else {
            let parsed = parse_command_frame(&input[cursor..], &Default::default())
                .map_err(|err| format!("owned frame parse failed: {err}"))?;
            write_owned_canonical(&mut out, &parsed.frame)?;
            cursor += parsed.consumed;
        }
    }
    io::stdout()
        .write_all(&out)
        .map_err(|err| format!("write golden: {err}"))
}

fn run_bench(
    input: &[u8],
    iterations: usize,
    field_count: usize,
    value_size: usize,
    borrowed: bool,
) -> Result<(), String> {
    let mut checksum = 0usize;
    let start = Instant::now();
    for _ in 0..iterations {
        let mut cursor = 0;
        while cursor < input.len() {
            if borrowed {
                let parsed =
                    parse_command_frame_borrowed(black_box(&input[cursor..]), &Default::default())
                        .map_err(|err| format!("borrowed frame parse failed: {err}"))?;
                if let BorrowedCommandFrame::Arguments(args) = &parsed.frame {
                    checksum = checksum.wrapping_add(args.len());
                    for arg in args {
                        checksum = checksum.wrapping_add(arg.len());
                    }
                }
                checksum = checksum.wrapping_add(parsed.consumed);
                cursor += parsed.consumed;
                black_box(parsed);
            } else {
                let parsed = parse_command_frame(black_box(&input[cursor..]), &Default::default())
                    .map_err(|err| format!("owned frame parse failed: {err}"))?;
                checksum = checksum.wrapping_add(parsed.consumed);
                cursor += parsed.consumed;
                black_box(parsed.frame);
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let frames = iterations * field_count;
    let mode = if borrowed {
        "borrowed-command"
    } else {
        "owned-command"
    };
    println!(
        "{{\"checksum\":{checksum},\"elapsed_seconds\":{elapsed:.9},\"fields\":{field_count},\"frames\":{frames},\"iterations\":{iterations},\"mode\":\"{mode}\",\"ops_per_second\":{:.3},\"value_size\":{value_size}}}",
        frames as f64 / elapsed
    );
    Ok(())
}

fn main() -> Result<(), String> {
    let iterations = std::env::var("ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(200_000);
    let field_count = std::env::var("FIELDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(16);
    let value_size = std::env::var("VALUE_SIZE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(64);
    let mode = std::env::var("MODE").unwrap_or_else(|_| "owned-command".to_string());
    let input = request_frame(field_count, value_size);
    match mode.as_str() {
        "owned-command" => run_bench(&input, iterations, field_count, value_size, false),
        "borrowed-command" => run_bench(&input, iterations, field_count, value_size, true),
        "golden-owned" => write_golden(&input, false),
        "golden-borrowed" => write_golden(&input, true),
        other => Err(format!("unknown MODE={other}")),
    }
}
