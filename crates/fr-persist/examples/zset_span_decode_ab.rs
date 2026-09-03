//! Callgrind A/B driver for the zset listpack span decode (`decode_zset_spans_and_scores`).
//!
//! Wall clock and even a server-level instruction ratio are unusable on this host right
//! now: the fleet sits past loadavg 100, and at that load the live-redis arm's serverCron
//! swings its own instruction count by nearly 2x, so the A/A null fails and no ratio is
//! quotable. A pure decode kernel measured by callgrind is deterministic and load-immune,
//! and a short-lived process running one arm has no cron, epoll or clock work at all --
//! which is why this driver's own A/A spread is zero rather than merely small.
//!
//! It does ONE thing per process: run one arm on one payload N times. Per-op cost comes
//! from the SLOPE, not a single run -- the same arm is measured at two op counts and the
//! totals differenced, so process startup, payload construction and teardown are identical
//! in both and cancel exactly.
//!
//!     zset_span_decode_ab <arm:pair|option> <members> <reps>
//!
//! Both arms MUST return identical results; the driver asserts that before timing, so a
//! divergent build cannot report a speedup.

use std::hint::black_box;

/// A `RDB_TYPE_ZSET_LISTPACK` body: member, score, member, score, ... Members are
/// letter-leading short strings and scores are small integers, which is the shape
/// `DUMP` produces for a listpack zset and the one the RESTORE ratio is taken on.
fn zset_listpack(members: u32) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    let mut count: u16 = 0;
    for i in 0..members {
        // Member: 6-bit string entry (0x80 | len), bytes, then the backlen byte.
        let member = format!("v{i:04}");
        body.push(0x80 | u8::try_from(member.len()).expect("short member"));
        body.extend_from_slice(member.as_bytes());
        body.push(u8::try_from(member.len() + 1).expect("short member"));
        // Score: 7-bit unsigned integer entry, then its backlen byte.
        body.push(u8::try_from(i & 0x7F).expect("7-bit"));
        body.push(1);
        count += 2;
    }
    // 6-byte header: total-bytes u32 LE, num-elements u16 LE. Then the 0xFF terminator.
    let total = u32::try_from(body.len() + 7).expect("fits");
    let mut lp = Vec::with_capacity(total as usize);
    lp.extend_from_slice(&total.to_le_bytes());
    lp.extend_from_slice(&count.to_le_bytes());
    lp.extend_from_slice(&body);
    lp.push(0xFF);
    lp
}

fn main() {
    let mut args = std::env::args().skip(1);
    let arm = args.next().unwrap_or_else(|| "pair".into());
    let members: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(40);
    let reps: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(2000);

    let lp = zset_listpack(members);

    // EQUIVALENCE BEFORE TIMING. A build where the arms disagree must not be able to
    // report a speedup.
    let a = fr_persist::listpack::decode_zset_spans_and_scores(&lp).expect("pair arm decodes");
    let b =
        fr_persist::listpack::decode_zset_spans_and_scores_orig(&lp).expect("option arm decodes");
    assert_eq!(a.len(), b.len(), "arms disagree on element count");
    for (index, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(
            x.0.as_bytes(&lp),
            y.0.as_bytes(&lp),
            "arms disagree on member {index}"
        );
        assert!(
            x.1.total_cmp(&y.1).is_eq(),
            "arms disagree on score {index}: {} vs {}",
            x.1,
            y.1
        );
    }

    let mut acc = 0usize;
    match arm.as_str() {
        "pair" => {
            for _ in 0..reps {
                let pairs = fr_persist::listpack::decode_zset_spans_and_scores(black_box(&lp))
                    .expect("decode");
                acc += black_box(pairs).len();
            }
        }
        "option" => {
            for _ in 0..reps {
                let pairs = fr_persist::listpack::decode_zset_spans_and_scores_orig(black_box(&lp))
                    .expect("decode");
                acc += black_box(pairs).len();
            }
        }
        other => panic!("unknown arm {other:?} (want pair|option)"),
    }
    println!("arm={arm} members={members} reps={reps} acc={acc}");
}
