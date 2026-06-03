use std::hint::black_box;
use std::io::{self, Write};
use std::time::Instant;

use fr_protocol::RespFrame;

fn golden_frames() -> Vec<RespFrame> {
    vec![
        RespFrame::SimpleString("OK".to_string()),
        RespFrame::SimpleString("PONG".to_string()),
        RespFrame::SimpleString("OK\r\nstill-one-frame".to_string()),
        RespFrame::Error("ERR sample".to_string()),
        RespFrame::Integer(1),
        RespFrame::Integer(0),
        RespFrame::BulkString(None),
        RespFrame::BulkString(Some(b"value".to_vec())),
        RespFrame::Array(Some(vec![
            RespFrame::SimpleString("OK".to_string()),
            RespFrame::Integer(7),
        ])),
    ]
}

fn write_golden() {
    let mut out = Vec::new();
    for frame in golden_frames() {
        frame.encode_into(&mut out);
    }
    io::stdout().write_all(&out).expect("write golden bytes");
}

fn run_ok_encode(iterations: usize) {
    let frame = RespFrame::SimpleString("OK".to_string());
    let mut out = Vec::with_capacity(16);
    let mut checksum = 0usize;
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(&frame).encode_into(&mut out);
        checksum = checksum.wrapping_add(out.len());
        black_box(&out);
        out.clear();
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{{\"checksum\":{checksum},\"elapsed_seconds\":{elapsed:.9},\"iterations\":{iterations},\"mode\":\"ok-encode-into\",\"ops_per_second\":{:.3}}}",
        iterations as f64 / elapsed
    );
}

fn main() {
    let iterations = std::env::var("ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(200_000_000);
    match std::env::var("MODE").as_deref() {
        Ok("golden") => write_golden(),
        _ => run_ok_encode(iterations),
    }
}
