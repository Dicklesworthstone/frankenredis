//! 6lgnu VALIDATION (not a product change): the NET structural question behind 6lgnu.
//!
//! fr's rank-queryable zset order = `BTreeMap` (order) + lazy treap (rank); its per-write rank tax was
//! measured at 1.28x(n5k)–1.55x(n20k) by `zadd_treap_tax`. 6lgnu proposes a single span-augmented
//! skiplist that fuses order+rank. This bench isolates the STRUCTURE: insert the SAME N pre-allocated
//! `(score, Arc<[u8]>)` members (Arc alloc excluded from timing; both arms just clone the Arc =
//! refcount) into:
//!   * ORIG arm = `BTreeMap<OrdKey,()>` — order only, NO rank (what fr's `ordered` costs WITHOUT the
//!     treap; it must ADD the 1.28-1.55x treap tax to answer ZRANK).
//!   * CAND arm = an arena span-augmented skiplist — order AND O(log n) rank in ONE insert.
//!
//! ratio = skiplist / btreemap. DECISION:
//!   * ratio < ~1.28  → skiplist gives rank for less than the treap tax BTreeMap must pay for it ⇒
//!                      6lgnu net-WINS the write side (and also drops a whole structure). PURSUE.
//!   * ratio >~ 1.55  → the skiplist's own span maintenance costs MORE than BTreeMap+treap ⇒ 6lgnu
//!                      does NOT win insert; only ZRANK read latency remains. DEPRIORITIZE.
//! The skiplist's rank is asserted against a sorted oracle, so the timed insert includes REAL span
//! maintenance (no free lunch from a rank-skipping prototype).
//!
//! Same cc harness: ONE binary, adjacent-pair interleave, black_box, reps calibrated once, median of
//! paired ratios, null-gated (btreemap vs btreemap), cv reported never gated.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

const ROUNDS: usize = 41;
const TARGET_SEGMENT_SECS: f64 = 0.010;
const NULL_LO: f64 = 0.05;
const NULL_HI: f64 = 0.95;

// ---- shared member set: distinct scores, inserted in a fixed pseudo-random order ----
fn members(n: usize) -> Vec<(f64, Arc<[u8]>)> {
    // distinct scores 0..n, member bytes stable; order permuted by a xorshift so inserts hit random
    // positions (not the sorted-append best case for either structure).
    let mut v: Vec<(f64, Arc<[u8]>)> = (0..n)
        .map(|i| (i as f64, Arc::from(format!("m{i:07}").into_bytes().into_boxed_slice())))
        .collect();
    let mut s = 0x2545_F491_4F6C_DD1D_u64 ^ (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for i in (1..v.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        v.swap(i, (s as usize) % (i + 1));
    }
    v
}

// ---- ORIG: BTreeMap ordered insert (order only, no rank) ----
#[derive(PartialEq)]
struct OrdKey(f64, Arc<[u8]>);
impl Eq for OrdKey {}
impl PartialOrd for OrdKey {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for OrdKey {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&o.0).then_with(|| self.1.as_ref().cmp(o.1.as_ref()))
    }
}

fn build_btreemap(items: &[(f64, Arc<[u8]>)]) -> usize {
    let mut m: BTreeMap<OrdKey, ()> = BTreeMap::new();
    for (sc, mem) in items {
        m.insert(OrdKey(*sc, Arc::clone(mem)), ());
    }
    m.len()
}

// ---- CAND: arena span-augmented skiplist (order + rank) ----
const MAX_LEVEL: usize = 24;
const NIL: usize = usize::MAX;

struct Node {
    score: f64,
    member: Arc<[u8]>,
    forward: [usize; MAX_LEVEL],
    span: [usize; MAX_LEVEL],
}

struct SkipList {
    nodes: Vec<Node>, // index 0 = header (member unused)
    level: usize,
    len: usize,
    rng: u64,
}

impl SkipList {
    fn new(cap: usize) -> Self {
        let mut nodes = Vec::with_capacity(cap + 1);
        nodes.push(Node {
            score: f64::NEG_INFINITY,
            member: Arc::from(Vec::new().into_boxed_slice()),
            forward: [NIL; MAX_LEVEL],
            span: [0; MAX_LEVEL],
        });
        SkipList { nodes, level: 1, len: 0, rng: 0x9E37_79B9_7F4A_7C15 }
    }

    #[inline]
    fn rand_level(&mut self) -> usize {
        // redis-style geometric with p = 1/4
        let mut lvl = 1;
        loop {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            if (self.rng & 3) != 0 || lvl >= MAX_LEVEL {
                break;
            }
            lvl += 1;
        }
        lvl
    }

    #[inline]
    fn key_lt(&self, idx: usize, score: f64, member: &[u8]) -> bool {
        let n = &self.nodes[idx];
        n.score < score || (n.score == score && n.member.as_ref() < member)
    }

    fn insert(&mut self, score: f64, member: Arc<[u8]>) {
        let mut update = [0usize; MAX_LEVEL];
        let mut rank = [0usize; MAX_LEVEL];
        let mut x = 0usize; // header
        for i in (0..self.level).rev() {
            rank[i] = if i == self.level - 1 { 0 } else { rank[i + 1] };
            while self.nodes[x].forward[i] != NIL
                && self.key_lt(self.nodes[x].forward[i], score, member.as_ref())
            {
                rank[i] += self.nodes[x].span[i];
                x = self.nodes[x].forward[i];
            }
            update[i] = x;
        }
        let new_level = self.rand_level();
        if new_level > self.level {
            for i in self.level..new_level {
                rank[i] = 0;
                update[i] = 0;
                self.nodes[0].span[i] = self.len; // header span at fresh level = current length
            }
            self.level = new_level;
        }
        let new_idx = self.nodes.len();
        let mut node = Node {
            score,
            member,
            forward: [NIL; MAX_LEVEL],
            span: [0; MAX_LEVEL],
        };
        for i in 0..new_level {
            let up = update[i];
            node.forward[i] = self.nodes[up].forward[i];
            node.span[i] = self.nodes[up].span[i] - (rank[0] - rank[i]);
            self.nodes[up].forward[i] = new_idx;
            self.nodes[up].span[i] = (rank[0] - rank[i]) + 1;
        }
        for i in new_level..self.level {
            self.nodes[update[i]].span[i] += 1;
        }
        self.nodes.push(node);
        self.len += 1;
    }

    /// 1-based rank of (score, member), or 0 if absent.
    fn rank_of(&self, score: f64, member: &[u8]) -> usize {
        let mut x = 0usize;
        let mut rank = 0usize;
        for i in (0..self.level).rev() {
            while self.nodes[x].forward[i] != NIL && {
                let f = self.nodes[x].forward[i];
                let n = &self.nodes[f];
                n.score < score || (n.score == score && n.member.as_ref() <= member)
            } {
                rank += self.nodes[x].span[i];
                x = self.nodes[x].forward[i];
            }
            if x != 0 && self.nodes[x].score == score && self.nodes[x].member.as_ref() == member {
                return rank;
            }
        }
        0
    }
}

fn build_skiplist(items: &[(f64, Arc<[u8]>)]) -> usize {
    let mut sl = SkipList::new(items.len());
    for (sc, mem) in items {
        sl.insert(*sc, Arc::clone(mem));
    }
    sl.len
}

fn median(r: &mut [f64]) -> f64 {
    r.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    r[r.len() / 2]
}
fn cv(r: &[f64]) -> f64 {
    let m = r.iter().sum::<f64>() / r.len() as f64;
    100.0 * (r.iter().map(|x| (x - m).powi(2)).sum::<f64>() / r.len() as f64).sqrt() / m
}
fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    // Correctness: the skiplist's span-based rank matches the sorted-order oracle for EVERY member,
    // so the timed insert genuinely maintained spans.
    {
        let items = members(2000);
        let mut sl = SkipList::new(items.len());
        for (sc, mem) in &items {
            sl.insert(*sc, Arc::clone(mem));
        }
        let mut sorted: Vec<&(f64, Arc<[u8]>)> = items.iter().collect();
        sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.as_ref().cmp(b.1.as_ref())));
        for (pos, (sc, mem)) in sorted.iter().enumerate() {
            assert_eq!(
                sl.rank_of(*sc, mem.as_ref()),
                pos + 1,
                "skiplist rank mismatch at sorted pos {pos}"
            );
        }
        assert_eq!(build_btreemap(&items), build_skiplist(&items), "cardinality mismatch");
    }

    println!(
        "\n{:<8} {:>7} {:>9} {:>16} {:>8} {:>14} {:>18}",
        "size", "reps", "NULL med", "null p5..p95", "null cv%", "skiplist/btree", "verdict(vs treap-tax)"
    );

    let sizes: &[(&str, usize)] = &[("n1k", 1_000), ("n5k", 5_000), ("n20k", 20_000)];
    for &(label, n) in sizes {
        let items = members(n);
        let time = |f: fn(&[(f64, Arc<[u8]>)]) -> usize, reps: usize| -> f64 {
            let start = Instant::now();
            let mut acc = 0usize;
            for _ in 0..reps {
                acc = acc.wrapping_add(f(black_box(items.as_slice())));
            }
            black_box(acc);
            start.elapsed().as_secs_f64()
        };

        let mut reps = 1usize;
        loop {
            let e = time(build_btreemap, reps);
            if e >= TARGET_SEGMENT_SECS || reps > 1 << 16 {
                reps = ((reps as f64) * (TARGET_SEGMENT_SECS / e.max(1e-9)).max(1.0)).ceil() as usize;
                break;
            }
            reps *= 2;
        }

        let mut nulls = Vec::with_capacity(ROUNDS);
        let mut ratios = Vec::with_capacity(ROUNDS);
        for round in 0..=ROUNDS {
            let swap = round % 2 == 1;
            let pair = |bf: fn(&[(f64, Arc<[u8]>)]) -> usize, cf: fn(&[(f64, Arc<[u8]>)]) -> usize| {
                if swap {
                    let c = time(cf, reps);
                    c / time(bf, reps)
                } else {
                    let b = time(bf, reps);
                    time(cf, reps) / b
                }
            };
            let nn = pair(build_btreemap, build_btreemap);
            let rt = pair(build_btreemap, build_skiplist);
            if round == 0 {
                continue;
            }
            nulls.push(nn);
            ratios.push(rt);
        }

        let null_med = median(&mut nulls);
        let ratio = median(&mut ratios);
        let lo = pct(&nulls, NULL_LO);
        let hi = pct(&nulls, NULL_HI);
        // treap-tax band from zadd_treap_tax (1.28 n5k .. 1.55 n20k); skiplist wins 6lgnu if below it.
        let verdict = if ratio < 1.28 {
            "6lgnu WINS insert"
        } else if ratio > 1.55 {
            "6lgnu loses insert"
        } else {
            "within-tax (marginal)"
        };
        println!(
            "{:<8} {:>7} {:>9.4} {:>16} {:>8.2} {:>13.3}x {:>18}",
            label,
            reps,
            null_med,
            format!("[{lo:.3}, {hi:.3}]"),
            cv(&nulls),
            ratio,
            verdict
        );
    }
}
