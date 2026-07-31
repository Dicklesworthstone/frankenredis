#![forbid(unsafe_code)]

//! Same-invocation A/A + A/B + live-incumbent gate for server throughput.
//!
//! The harness deliberately drives many established connections from persistent
//! client shards. Every shard writes its clients before reading their replies;
//! independent shards overlap so pipeline depth 1 can saturate the server rather
//! than the client. By default, two byte-identical mio processes provide the
//! null control and the same FrankenRedis ELF with the runtime flag is the
//! candidate. A command-shape experiment may instead put all three FrankenRedis
//! processes on io_uring and select a frozen control route by environment before
//! the first packet. Vendored Redis is always the live incumbent.
//!
//! Run only through strict remote RCH on one explicitly selected worker:
//!
//! `RCH_WORKER=<worker> RCH_REQUIRE_REMOTE=1 env -u CARGO_TARGET_DIR rch exec --
//! cargo test --profile release-perf -p fr-server --features io-uring-writes
//! --test io_uring_submission_ab -- --ignored --exact
//! io_uring_submission_same_elf_null_then_ab --nocapture --test-threads=1`

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_CLIENTS: usize = 50;
const MAX_CLIENT_THREADS: usize = 128;
// Four shards became client-bound below one microsecond per command at P16, and
// even five left the ECHO floor at only 85.298% median server utilization. Nine
// shards use the worker's remaining physical cores while keeping a disjoint
// server core; the utilization guard remains authoritative.
const DEFAULT_CLIENT_THREADS: usize = 9;
// Two complete 24-permutation order cycles keep every physical arm in every
// position equally often. A partial tail can bias an otherwise identical A/A
// pair; the median validity check caught that on the first XTRIM floor run.
const DEFAULT_SAMPLES: usize = 48;
const DEFAULT_OPS_PER_SAMPLE: usize = 200_000;
const DEFAULT_PROFILE_SECONDS: u64 = 3;
// A slow incumbent full-pass command can legitimately need more than ten
// seconds to drain one P16 group. This is only a hang bound: timed wall
// duration still includes every byte read below.
const BENCH_CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(60);
// One group is only CLIENTS * pipeline operations. Twenty-five groups left a
// sub-microsecond floor dominated by client-channel handoffs even with nine
// pinned shards; 125 keeps each arm slice below one second while amortizing the
// barrier enough to drive the server continuously.
const DEFAULT_INTERLEAVE_GROUPS: usize = 125;
// The trj sweep uses 128 clients at P16, so 200k requested operations are only
// 98 client groups. Sixteen groups keep seven A/A/A/B/incumbent rotations
// inside each full sample instead of degenerating to one sequential arm block.
const SHARDED_DEFAULT_INTERLEAVE_GROUPS: usize = 16;
const QUIET_CORE_MAX_PCT: f64 = 5.0;
const QUIET_CORE_PREFLIGHT_ATTEMPTS: usize = 20;
// Host-wide scaling is admissible only when the entire original process
// cpuset is quiet. This mirrors FrankenFS's fail-closed trj contract: picking
// a few quiet cores does not prove that another benchmark is not consuming
// the rest of the machine.
const HOST_WIDE_MAX_BUSY_PCT: f64 = 20.0;
const MIN_SERVER_UTIL_PCT: f64 = 90.0;
const HOT_LIBC_LEAF_MIN_SELF_PCT: f64 = 5.0;
const IO_URING_FLAG: &str = "--io-uring-output";
const BITPOS_RANGE_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_BITPOS_RANGE_FLOOR_ORIG";
const BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_BITFIELD_RO_TWO_GET_FLOOR_ORIG";
const OBJECT_ENCODING_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_OBJECT_ENCODING_FLOOR_ORIG";
const OBJECT_REFCOUNT_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_OBJECT_REFCOUNT_FLOOR_ORIG";
const DBSIZE_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_DBSIZE_FLOOR_ORIG";
const ECHO_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_ECHO_FLOOR_ORIG";
const WAIT_ZERO_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_WAIT_ZERO_FLOOR_ORIG";
const XTRIM_MINID_NOOP_CONTROL_ENV: &str = "FR_PERF_AB_XTRIM_MINID_NOOP_ORIG";
const XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_XTRIM_MINID_NOOP_FLOOR_ORIG";
const XDEL_MISSING_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_XDEL_MISSING_FLOOR_ORIG";
const XACK_MISSING_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_XACK_MISSING_FLOOR_ORIG";
const XRANGE_ZERO_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_XRANGE_ZERO_FLOOR_ORIG";
const XREVRANGE_ZERO_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_XREVRANGE_ZERO_FLOOR_ORIG";
const LRANGE_FLOOR_CONTROL_ENV: &str = "FR_PERF_AB_LRANGE_FLOOR_ORIG";
const XTRIM_MINID_NOOP_PREFILL_ENTRIES: usize = 1_000;
const SHUTDOWN: &[u8] = b"*2\r\n$8\r\nSHUTDOWN\r\n$6\r\nNOSAVE\r\n";
const SET: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
const SET_REPLY: &[u8] = b"+OK\r\n";
const GET: &[u8] = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";
const GET_REPLY: &[u8] = b"$1\r\nv\r\n";
const PTTL_PERSISTENT: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$1\r\nk\r\n";
const PTTL_PERSISTENT_REPLY: &[u8] = b":-1\r\n";
const ZREMRANGEBYSCORE_INVERTED_PREFILL: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nz\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n$1\r\nz\r\n\
*4\r\n$4\r\nZADD\r\n$1\r\nz\r\n$1\r\n1\r\n$1\r\nm\r\n";
const ZREMRANGEBYSCORE_INVERTED_PREFILL_REPLY: &[u8] = b"+OK\r\n:1\r\n:1\r\n";
const ZREMRANGEBYSCORE_INVERTED: &[u8] =
    b"*4\r\n$16\r\nZREMRANGEBYSCORE\r\n$1\r\nz\r\n$4\r\n+inf\r\n$4\r\n-inf\r\n";
const ZREMRANGEBYSCORE_INVERTED_REPLY: &[u8] = b":0\r\n";
const ZREMRANGEBYRANK_NOOP: &[u8] =
    b"*4\r\n$15\r\nZREMRANGEBYRANK\r\n$4\r\nlexz\r\n$4\r\n2500\r\n$4\r\n3500\r\n";
const ZREMRANGEBYRANK_NOOP_REPLY: &[u8] = b":0\r\n";
const ZREMRANGEBYLEX_NOOP: &[u8] =
    b"*4\r\n$14\r\nZREMRANGEBYLEX\r\n$4\r\nlexz\r\n$6\r\n[02500\r\n$6\r\n[03500\r\n";
const ZREMRANGEBYLEX_NOOP_REPLY: &[u8] = b":0\r\n";
const ZRANGEBYLEX_MEMBERS: usize = 2_000;
const ZRANGEBYLEX_RANGE_START: usize = 500;
const ZRANGEBYLEX_RANGE_END: usize = 1_500;
const ZRANGEBYLEX_PREFILL_BATCH: usize = 256;
const ZRANGEBYLEX_RANGE: &[u8] =
    b"*4\r\n$11\r\nZRANGEBYLEX\r\n$4\r\nlexz\r\n$6\r\n[00500\r\n$6\r\n[01500\r\n";
const ZREVRANGEBYLEX_RANGE: &[u8] =
    b"*4\r\n$14\r\nZREVRANGEBYLEX\r\n$4\r\nlexz\r\n$6\r\n[01500\r\n$6\r\n[00500\r\n";
const ZRANGEBYSCORE_MEMBERS: usize = 2_000;
const ZRANGEBYSCORE_RANGE_START: usize = 500;
const ZRANGEBYSCORE_RANGE_END: usize = 1_500;
const ZRANGEBYSCORE_PREFILL_BATCH: usize = 256;
const ZRANGEBYSCORE_RANGE: &[u8] =
    b"*4\r\n$13\r\nZRANGEBYSCORE\r\n$6\r\nscorez\r\n$3\r\n500\r\n$4\r\n1500\r\n";
const ZREVRANGEBYSCORE_RANGE: &[u8] =
    b"*4\r\n$16\r\nZREVRANGEBYSCORE\r\n$6\r\nscorez\r\n$4\r\n1500\r\n$3\r\n500\r\n";
const ZREVRANGE_RANK_START: usize = 499;
const ZREVRANGE_RANK_END: usize = 1_499;
const ZREVRANGE_RANK_RANGE: &[u8] =
    b"*4\r\n$9\r\nZREVRANGE\r\n$6\r\nscorez\r\n$3\r\n499\r\n$4\r\n1499\r\n";
const ZRANGEBYSCORE_ZCARD: &[u8] = b"*2\r\n$5\r\nZCARD\r\n$6\r\nscorez\r\n";
const ZRANGEBYSCORE_ZCARD_REPLY: &[u8] = b":2000\r\n";
const ZRANGEBYSCORE_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$6\r\nscorez\r\n";
const ZRANGEBYSCORE_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$6\r\nscorez\r\n";
const ZRANGEBYLEX_ZCARD: &[u8] = b"*2\r\n$5\r\nZCARD\r\n$4\r\nlexz\r\n";
const ZRANGEBYLEX_ZCARD_REPLY: &[u8] = b":2000\r\n";
const ZRANGEBYLEX_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$4\r\nlexz\r\n";
const ZRANGEBYLEX_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$4\r\nlexz\r\n";
const ZSET_SKIPLIST_ENCODING_REPLY: &[u8] = b"$8\r\nskiplist\r\n";
const LRANGE_INVERTED_PREFILL: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nl\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n$1\r\nl\r\n\
*3\r\n$5\r\nLPUSH\r\n$1\r\nl\r\n$1\r\nm\r\n";
const LRANGE_INVERTED_PREFILL_REPLY: &[u8] = b"+OK\r\n:1\r\n:1\r\n";
const LRANGE_INVERTED: &[u8] = b"*4\r\n$6\r\nLRANGE\r\n$1\r\nl\r\n$1\r\n1\r\n$1\r\n0\r\n";
const LRANGE_INVERTED_REPLY: &[u8] = b"*0\r\n";
const LPOP_COUNT_MISSING: &[u8] = b"*3\r\n$4\r\nLPOP\r\n$12\r\nmissing-list\r\n$1\r\n2\r\n";
const RPOP_COUNT_MISSING: &[u8] = b"*3\r\n$4\r\nRPOP\r\n$12\r\nmissing-list\r\n$1\r\n2\r\n";
const LIST_COUNT_MISSING_REPLY: &[u8] = b"*-1\r\n";
const LIST_COUNT_MISSING_EXISTS: &[u8] = b"*2\r\n$6\r\nEXISTS\r\n$12\r\nmissing-list\r\n";
const LIST_COUNT_MISSING_EXISTS_REPLY: &[u8] = b":0\r\n";
const LIST_COUNT_MISSING_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$12\r\nmissing-list\r\n";
const LIST_COUNT_MISSING_PTTL_REPLY: &[u8] = b":-2\r\n";
const LINDEX_MIDDLE_ELEMENTS: usize = 500;
const LINDEX_MIDDLE: &[u8] = b"*3\r\n$6\r\nLINDEX\r\n$1\r\nl\r\n$3\r\n250\r\n";
const LINDEX_MIDDLE_REPLY: &[u8] = b"$4\r\nv250\r\n";
const LSET_MIDDLE_SAME_VALUE: &[u8] = b"*4\r\n$4\r\nLSET\r\n$1\r\nl\r\n$3\r\n250\r\n$4\r\nv250\r\n";
const LSET_MIDDLE_SAME_VALUE_REPLY: &[u8] = b"+OK\r\n";
const LPOS_MIDDLE_ELEMENT: &[u8] = b"*3\r\n$4\r\nLPOS\r\n$1\r\nl\r\n$4\r\nv250\r\n";
const LPOS_MIDDLE_ELEMENT_REPLY: &[u8] = b":250\r\n";
const LINDEX_MIDDLE_LLEN: &[u8] = b"*2\r\n$4\r\nLLEN\r\n$1\r\nl\r\n";
const LINDEX_MIDDLE_LLEN_REPLY: &[u8] = b":500\r\n";
const LINDEX_MIDDLE_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$1\r\nl\r\n";
const LINDEX_MIDDLE_REDIS_ENCODING_REPLY: &[u8] = b"$8\r\nlistpack\r\n";
const LINDEX_MIDDLE_CONFIG: &[u8] =
    b"*3\r\n$6\r\nCONFIG\r\n$3\r\nGET\r\n$22\r\nlist-max-listpack-size\r\n";
const LINDEX_MIDDLE_REDIS_CONFIG_REPLY: &[u8] =
    b"*2\r\n$22\r\nlist-max-listpack-size\r\n$2\r\n-2\r\n";
const MISSING_FIELD_HASH_FIELDS: usize = 500;
const HDEL_MISSING_FIELD: &[u8] = b"*3\r\n$4\r\nHDEL\r\n$1\r\nh\r\n$6\r\nabsent\r\n";
const HDEL_MISSING_FIELD_REPLY: &[u8] = b":0\r\n";
const HGET_MISSING_FIELD: &[u8] = b"*3\r\n$4\r\nHGET\r\n$1\r\nh\r\n$6\r\nabsent\r\n";
const HGET_MISSING_FIELD_REPLY: &[u8] = b"$-1\r\n";
const HEXISTS_MISSING_FIELD: &[u8] = b"*3\r\n$7\r\nHEXISTS\r\n$1\r\nh\r\n$6\r\nabsent\r\n";
const HEXISTS_MISSING_FIELD_REPLY: &[u8] = b":0\r\n";
const HKEYS_FIELDS: &[u8] = b"*2\r\n$5\r\nHKEYS\r\n$1\r\nh\r\n";
const HVALS_FIELDS: &[u8] = b"*2\r\n$5\r\nHVALS\r\n$1\r\nh\r\n";
const HGETALL_FIELDS: &[u8] = b"*2\r\n$7\r\nHGETALL\r\n$1\r\nh\r\n";
const HSCAN_ALL_FIELDS: &[u8] =
    b"*5\r\n$5\r\nHSCAN\r\n$1\r\nh\r\n$1\r\n0\r\n$5\r\nCOUNT\r\n$4\r\n1000\r\n";
const SCAN0_MEMBERS: usize = 16;
const SSCAN_CURSOR_ZERO: &[u8] = b"*3\r\n$5\r\nSSCAN\r\n$2\r\ns0\r\n$1\r\n0\r\n";
const HSCAN_CURSOR_ZERO: &[u8] = b"*3\r\n$5\r\nHSCAN\r\n$2\r\nh0\r\n$1\r\n0\r\n";
const ZSCAN_CURSOR_ZERO: &[u8] = b"*3\r\n$5\r\nZSCAN\r\n$2\r\nz0\r\n$1\r\n0\r\n";
const SCAN0_CARD_REPLY: &[u8] = b":16\r\n";
const SSCAN0_CARD: &[u8] = b"*2\r\n$5\r\nSCARD\r\n$2\r\ns0\r\n";
const HSCAN0_CARD: &[u8] = b"*2\r\n$4\r\nHLEN\r\n$2\r\nh0\r\n";
const ZSCAN0_CARD: &[u8] = b"*2\r\n$5\r\nZCARD\r\n$2\r\nz0\r\n";
const SSCAN0_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$2\r\ns0\r\n";
const HSCAN0_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$2\r\nh0\r\n";
const ZSCAN0_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$2\r\nz0\r\n";
const SSCAN0_ENCODING_REPLY: &[u8] = b"$6\r\nintset\r\n";
const SCAN0_LISTPACK_ENCODING_REPLY: &[u8] = b"$8\r\nlistpack\r\n";
const SSCAN0_BOUNDARIES: &[u8] = b"*6\r\n$11\r\nSMISMEMBER\r\n$2\r\ns0\r\n\
$2\r\n-1\r\n$1\r\n0\r\n$2\r\n15\r\n$2\r\n16\r\n";
const SSCAN0_BOUNDARIES_REPLY: &[u8] = b"*4\r\n:0\r\n:1\r\n:1\r\n:0\r\n";
const HSCAN0_BOUNDARIES: &[u8] =
    b"*5\r\n$5\r\nHMGET\r\n$2\r\nh0\r\n$3\r\nf00\r\n$3\r\nf15\r\n$6\r\nabsent\r\n";
const HSCAN0_BOUNDARIES_REPLY: &[u8] = b"*3\r\n$3\r\nv00\r\n$3\r\nv15\r\n$-1\r\n";
const ZSCAN0_BOUNDARIES: &[u8] =
    b"*5\r\n$7\r\nZMSCORE\r\n$2\r\nz0\r\n$3\r\nm00\r\n$3\r\nm15\r\n$6\r\nabsent\r\n";
const ZSCAN0_BOUNDARIES_REPLY: &[u8] = b"*3\r\n$1\r\n0\r\n$2\r\n15\r\n$-1\r\n";
const SSCAN0_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$2\r\ns0\r\n";
const HSCAN0_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$2\r\nh0\r\n";
const ZSCAN0_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$2\r\nz0\r\n";
const HSET_SAME_VALUE: &[u8] = b"*4\r\n$4\r\nHSET\r\n$1\r\nh\r\n$4\r\nf250\r\n$1\r\n1\r\n";
const HSET_SAME_VALUE_REPLY: &[u8] = b":0\r\n";
const HSETNX_EXISTING_FIELD: &[u8] = b"*4\r\n$6\r\nHSETNX\r\n$1\r\nh\r\n$4\r\nf250\r\n$1\r\nv\r\n";
const HSETNX_EXISTING_FIELD_REPLY: &[u8] = b":0\r\n";
const HINCRBY_ZERO_DELTA: &[u8] = b"*4\r\n$7\r\nHINCRBY\r\n$1\r\nh\r\n$4\r\nf250\r\n$1\r\n0\r\n";
const HINCRBY_ZERO_DELTA_REPLY: &[u8] = b":1\r\n";
const HINCRBYFLOAT_ZERO_DELTA: &[u8] =
    b"*4\r\n$12\r\nHINCRBYFLOAT\r\n$1\r\nh\r\n$4\r\nf250\r\n$1\r\n0\r\n";
const HINCRBYFLOAT_ZERO_DELTA_REPLY: &[u8] = b"$1\r\n1\r\n";
const HSTRLEN_EXISTING_FIELD: &[u8] = b"*3\r\n$7\r\nHSTRLEN\r\n$1\r\nh\r\n$4\r\nf250\r\n";
const HSTRLEN_EXISTING_FIELD_REPLY: &[u8] = b":1\r\n";
const HMGET_EXISTING_MISSING: &[u8] =
    b"*4\r\n$5\r\nHMGET\r\n$1\r\nh\r\n$4\r\nf250\r\n$6\r\nabsent\r\n";
const HMGET_EXISTING_MISSING_REPLY: &[u8] = b"*2\r\n$1\r\nv\r\n$-1\r\n";
const PFMERGE_DENSE_SOURCE_ELEMENTS: usize = 4_096;
const PFMERGE_DENSE_PREFILL_BATCH: usize = 256;
const PFMERGE_TWO_DENSE: &[u8] = b"*4\r\n$7\r\nPFMERGE\r\n$3\r\ndst\r\n$2\r\nh1\r\n$2\r\nh2\r\n";
const PFMERGE_TWO_DENSE_REPLY: &[u8] = b"+OK\r\n";
const PFCOUNT_TWO_DENSE: &[u8] = b"*3\r\n$7\r\nPFCOUNT\r\n$2\r\nh1\r\n$2\r\nh2\r\n";
const PFCOUNT_TWO_DENSE_REPLY: &[u8] = b":8173\r\n";
const BITCOUNT_ONE_MIB_BYTES: usize = 1 << 20;
const BITCOUNT_ONE_MIB: &[u8] = b"*2\r\n$8\r\nBITCOUNT\r\n$10\r\nbitcount:k\r\n";
const BITCOUNT_ONE_MIB_REPLY: &[u8] = b":4194304\r\n";
const BITCOUNT_ONE_MIB_GET: &[u8] = b"*2\r\n$3\r\nGET\r\n$10\r\nbitcount:k\r\n";
const BITCOUNT_ONE_MIB_STRLEN: &[u8] = b"*2\r\n$6\r\nSTRLEN\r\n$10\r\nbitcount:k\r\n";
const BITCOUNT_ONE_MIB_STRLEN_REPLY: &[u8] = b":1048576\r\n";
const BITCOUNT_ONE_MIB_ENCODING: &[u8] =
    b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$10\r\nbitcount:k\r\n";
const BITCOUNT_ONE_MIB_ENCODING_REPLY: &[u8] = b"$3\r\nraw\r\n";
const SUNIONSTORE_SMALL_MEMBERS: usize = 512;
const SUNIONSTORE_LARGE_MEMBERS: usize = 4_096;
const SUNIONSTORE_LARGE_START: usize = 10_000;
const SUNIONSTORE_PREFILL_BATCH: usize = 256;
const SUNIONSTORE_MIXED: &[u8] =
    b"*4\r\n$11\r\nSUNIONSTORE\r\n$3\r\ndst\r\n$5\r\nsmall\r\n$10\r\nlarge_miss\r\n";
const SUNIONSTORE_MIXED_REPLY: &[u8] = b":4608\r\n";
const SUNIONSTORE_SMALL_SCARD: &[u8] = b"*2\r\n$5\r\nSCARD\r\n$5\r\nsmall\r\n";
const SUNIONSTORE_SMALL_SCARD_REPLY: &[u8] = b":512\r\n";
const SUNIONSTORE_LARGE_SCARD: &[u8] = b"*2\r\n$5\r\nSCARD\r\n$10\r\nlarge_miss\r\n";
const SUNIONSTORE_LARGE_SCARD_REPLY: &[u8] = b":4096\r\n";
const SUNIONSTORE_DST_SCARD: &[u8] = b"*2\r\n$5\r\nSCARD\r\n$3\r\ndst\r\n";
const SUNIONSTORE_DST_SCARD_REPLY: &[u8] = b":4608\r\n";
const SUNIONSTORE_SMALL_ENCODING: &[u8] =
    b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$5\r\nsmall\r\n";
const SUNIONSTORE_SMALL_ENCODING_REPLY: &[u8] = b"$6\r\nintset\r\n";
const SUNIONSTORE_LARGE_ENCODING: &[u8] =
    b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$10\r\nlarge_miss\r\n";
const SUNIONSTORE_DST_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$3\r\ndst\r\n";
const SUNIONSTORE_HASHTABLE_ENCODING_REPLY: &[u8] = b"$9\r\nhashtable\r\n";
const SUNIONSTORE_DST_MEMBERSHIP: &[u8] = b"*8\r\n$10\r\nSMISMEMBER\r\n$3\r\ndst\r\n\
$1\r\n0\r\n$3\r\n511\r\n$4\r\n9999\r\n$5\r\n10000\r\n$5\r\n14095\r\n$5\r\n14096\r\n";
const SUNIONSTORE_DST_MEMBERSHIP_REPLY: &[u8] = b"*6\r\n:1\r\n:1\r\n:0\r\n:1\r\n:1\r\n:0\r\n";
const SUNIONSTORE_DST_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$3\r\ndst\r\n";
const SDIFFSTORE_MIXED: &[u8] =
    b"*4\r\n$10\r\nSDIFFSTORE\r\n$3\r\ndst\r\n$5\r\nsmall\r\n$10\r\nlarge_miss\r\n";
const SDIFFSTORE_MIXED_REPLY: &[u8] = b":512\r\n";
const SDIFFSTORE_DST_SCARD_REPLY: &[u8] = b":512\r\n";
const SDIFFSTORE_DST_MEMBERSHIP: &[u8] = b"*8\r\n$10\r\nSMISMEMBER\r\n$3\r\ndst\r\n\
$1\r\n0\r\n$3\r\n256\r\n$3\r\n511\r\n$3\r\n512\r\n$5\r\n10000\r\n$5\r\n14095\r\n";
const SDIFFSTORE_DST_MEMBERSHIP_REPLY: &[u8] = b"*6\r\n:1\r\n:1\r\n:1\r\n:0\r\n:0\r\n:0\r\n";
const SINTERSTORE_MIXED: &[u8] =
    b"*4\r\n$11\r\nSINTERSTORE\r\n$3\r\ndst\r\n$5\r\nsmall\r\n$5\r\nlarge\r\n";
const SINTERSTORE_MIXED_REPLY: &[u8] = b":512\r\n";
const SINTERSTORE_LARGE_SCARD: &[u8] = b"*2\r\n$5\r\nSCARD\r\n$5\r\nlarge\r\n";
const SINTERSTORE_LARGE_ENCODING: &[u8] =
    b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$5\r\nlarge\r\n";
const SINTERSTORE_DST_MEMBERSHIP: &[u8] = b"*8\r\n$10\r\nSMISMEMBER\r\n$3\r\ndst\r\n\
$1\r\n0\r\n$3\r\n256\r\n$3\r\n511\r\n$3\r\n512\r\n$4\r\n4095\r\n$4\r\n4096\r\n";
const SINTERSTORE_DST_MEMBERSHIP_REPLY: &[u8] = b"*6\r\n:1\r\n:1\r\n:1\r\n:0\r\n:0\r\n:0\r\n";
const ZINTERCARD_SOURCE_MEMBERS: usize = 4_096;
const ZINTERCARD_SOURCE_B_START: usize = 2_048;
const ZINTERCARD_INTERSECTION_MEMBERS: usize = 2_048;
const ZINTERCARD_PREFILL_BATCH: usize = 256;
const ZINTERCARD_CACHED: &[u8] = b"*4\r\n$10\r\nZINTERCARD\r\n$1\r\n2\r\n$2\r\nza\r\n$2\r\nzb\r\n";
const ZINTERCARD_CACHED_REPLY: &[u8] = b":2048\r\n";
const ZDIFF_TWO_KEY: &[u8] = b"*4\r\n$5\r\nZDIFF\r\n$1\r\n2\r\n$2\r\nza\r\n$2\r\nzb\r\n";
const ZINTER_TWO_KEY: &[u8] = b"*4\r\n$6\r\nZINTER\r\n$1\r\n2\r\n$2\r\nza\r\n$2\r\nzb\r\n";
const ZINTERCARD_ZA_CARD: &[u8] = b"*2\r\n$5\r\nZCARD\r\n$2\r\nza\r\n";
const ZINTERCARD_ZB_CARD: &[u8] = b"*2\r\n$5\r\nZCARD\r\n$2\r\nzb\r\n";
const ZINTERCARD_SOURCE_CARD_REPLY: &[u8] = b":4096\r\n";
const ZINTERCARD_ZA_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$2\r\nza\r\n";
const ZINTERCARD_ZB_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$2\r\nzb\r\n";
const ZINTERCARD_SKIPLIST_ENCODING_REPLY: &[u8] = b"$8\r\nskiplist\r\n";
const ZINTERCARD_ZA_BOUNDARIES: &[u8] = b"*8\r\n$7\r\nZMSCORE\r\n$2\r\nza\r\n\
$1\r\n0\r\n$4\r\n2047\r\n$4\r\n2048\r\n$4\r\n4095\r\n$4\r\n4096\r\n$4\r\n6143\r\n";
const ZINTERCARD_ZA_BOUNDARIES_REPLY: &[u8] =
    b"*6\r\n$1\r\n0\r\n$4\r\n2047\r\n$4\r\n2048\r\n$4\r\n4095\r\n$-1\r\n$-1\r\n";
const ZINTERCARD_ZB_BOUNDARIES: &[u8] = b"*9\r\n$7\r\nZMSCORE\r\n$2\r\nzb\r\n\
$1\r\n0\r\n$4\r\n2047\r\n$4\r\n2048\r\n$4\r\n4095\r\n$4\r\n4096\r\n$4\r\n6143\r\n$4\r\n6144\r\n";
const ZINTERCARD_ZB_BOUNDARIES_REPLY: &[u8] = b"*7\r\n$-1\r\n$-1\r\n$4\r\n2048\r\n\
$4\r\n4095\r\n$4\r\n4096\r\n$4\r\n6143\r\n$-1\r\n";
const ZINTERCARD_ZA_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$2\r\nza\r\n";
const ZINTERCARD_ZB_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$2\r\nzb\r\n";
const PFMERGE_H1_ENCODING: &[u8] = b"*3\r\n$7\r\nPFDEBUG\r\n$8\r\nENCODING\r\n$2\r\nh1\r\n";
const PFMERGE_H2_ENCODING: &[u8] = b"*3\r\n$7\r\nPFDEBUG\r\n$8\r\nENCODING\r\n$2\r\nh2\r\n";
const PFMERGE_DST_ENCODING: &[u8] = b"*3\r\n$7\r\nPFDEBUG\r\n$8\r\nENCODING\r\n$3\r\ndst\r\n";
const PFMERGE_DENSE_ENCODING_REPLY: &[u8] = b"+dense\r\n";
const PFMERGE_DST_COUNT: &[u8] = b"*2\r\n$7\r\nPFCOUNT\r\n$3\r\ndst\r\n";
const PFMERGE_DST_COUNT_REPLY: &[u8] = b":8173\r\n";
const MISSING_FIELD_HASH_HLEN: &[u8] = b"*2\r\n$4\r\nHLEN\r\n$1\r\nh\r\n";
const MISSING_FIELD_HASH_HLEN_REPLY: &[u8] = b":500\r\n";
const MISSING_FIELD_HASH_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$1\r\nh\r\n";
const MISSING_FIELD_HASH_REDIS_ENCODING_REPLY: &[u8] = b"$8\r\nlistpack\r\n";
const BITPOS_RANGE_PREFILL: &[u8] =
    b"*3\r\n$3\r\nSET\r\n$8\r\nbitpos:k\r\n$8\r\n\0\0\0\0\0\0\0\x80\r\n";
const BITPOS_RANGE: &[u8] =
    b"*5\r\n$6\r\nBITPOS\r\n$8\r\nbitpos:k\r\n$1\r\n1\r\n$1\r\n0\r\n$1\r\n7\r\n";
const BITPOS_RANGE_REPLY: &[u8] = b":56\r\n";
const BITFIELD_RO_TWO_GET_PREFILL: &[u8] =
    b"*3\r\n$3\r\nSET\r\n$10\r\nbitfield:k\r\n$2\r\n\x12\x34\r\n";
const BITFIELD_RO_TWO_GET: &[u8] = b"*8\r\n$11\r\nBITFIELD_RO\r\n$10\r\nbitfield:k\r\n\
$3\r\nGET\r\n$2\r\nu8\r\n$1\r\n0\r\n$3\r\nGET\r\n$2\r\nu8\r\n$1\r\n8\r\n";
const BITFIELD_RO_TWO_GET_REPLY: &[u8] = b"*2\r\n:18\r\n:52\r\n";
const OBJECT_ENCODING_PREFILL: &[u8] = b"*3\r\n$3\r\nSET\r\n$8\r\nobject:k\r\n$2\r\n42\r\n";
const OBJECT_ENCODING: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nENCODING\r\n$8\r\nobject:k\r\n";
const OBJECT_ENCODING_REPLY: &[u8] = b"$3\r\nint\r\n";
const OBJECT_REFCOUNT_PREFILL: &[u8] = b"*3\r\n$3\r\nSET\r\n$8\r\nobject:k\r\n$5\r\nvalue\r\n";
const OBJECT_REFCOUNT: &[u8] = b"*3\r\n$6\r\nOBJECT\r\n$8\r\nREFCOUNT\r\n$8\r\nobject:k\r\n";
const OBJECT_REFCOUNT_REPLY: &[u8] = b":1\r\n";
const DBSIZE: &[u8] = b"*1\r\n$6\r\nDBSIZE\r\n";
const DBSIZE_REPLY: &[u8] = b":1\r\n";
// Keep the incumbent's full-pass work per measured group constant while the
// saturated P16 geometry uses 400 connections: 12,500 keys * 6,400 commands
// matches the former 50,000 keys * 1,600 commands. The longer candidate block
// amortizes client-driver handoff noise without making Redis timing cheaper.
const SELECTIVE_PREFIX_DECOYS: usize = 12_500;
const SELECTIVE_PREFIX_PREFILL_BATCH: usize = 256;
const KEYS_SELECTIVE_PREFIX: &[u8] = b"*2\r\n$4\r\nKEYS\r\n$15\r\ntenant:needle:*\r\n";
const KEYS_SELECTIVE_PREFIX_REPLY: &[u8] = b"*1\r\n$20\r\ntenant:needle:000000\r\n";
const SELECTIVE_PREFIX_PTTL: &[u8] = b"*2\r\n$4\r\nPTTL\r\n$20\r\ntenant:needle:000000\r\n";
const SCAN_SELECTIVE_PREFIX: &[u8] = b"*6\r\n$4\r\nSCAN\r\n$1\r\n0\r\n$5\r\nMATCH\r\n\
$15\r\ntenant:needle:*\r\n$5\r\nCOUNT\r\n$6\r\n100000\r\n";
const SCAN_SELECTIVE_PREFIX_REPLY: &[u8] =
    b"*2\r\n$1\r\n0\r\n*1\r\n$20\r\ntenant:needle:000000\r\n";
const ECHO: &[u8] = b"*2\r\n$4\r\nECHO\r\n$1\r\nx\r\n";
const ECHO_REPLY: &[u8] = b"$1\r\nx\r\n";
const UNWATCH: &[u8] = b"*1\r\n$7\r\nUNWATCH\r\n";
const UNWATCH_REPLY: &[u8] = b"+OK\r\n";
const WAIT_ZERO: &[u8] = b"*3\r\n$4\r\nWAIT\r\n$1\r\n0\r\n$1\r\n0\r\n";
const WAIT_ZERO_REPLY: &[u8] = b":0\r\n";
const XTRIM_MINID_NOOP: &[u8] =
    b"*5\r\n$5\r\nXTRIM\r\n$2\r\nxs\r\n$5\r\nMINID\r\n$1\r\n~\r\n$3\r\n0-0\r\n";
const XTRIM_MINID_NOOP_REPLY: &[u8] = b":0\r\n";
const XDEL_MISSING: &[u8] = b"*3\r\n$4\r\nXDEL\r\n$2\r\nxs\r\n$3\r\n0-0\r\n";
const XDEL_MISSING_REPLY: &[u8] = b":0\r\n";
const XACK_MISSING: &[u8] = b"*4\r\n$4\r\nXACK\r\n$2\r\nxs\r\n$1\r\ng\r\n$3\r\n0-0\r\n";
const XACK_MISSING_REPLY: &[u8] = b":0\r\n";
const XCLAIM_MISSING: &[u8] =
    b"*6\r\n$6\r\nXCLAIM\r\n$2\r\nxs\r\n$1\r\ng\r\n$1\r\nc\r\n$1\r\n0\r\n$3\r\n0-0\r\n";
const XCLAIM_MISSING_REPLY: &[u8] = b"*0\r\n";
const XRANGE_ZERO: &[u8] = b"*4\r\n$6\r\nXRANGE\r\n$2\r\nxs\r\n$3\r\n0-0\r\n$3\r\n0-0\r\n";
const XRANGE_ZERO_REPLY: &[u8] = b"*0\r\n";
const XREVRANGE_ZERO: &[u8] = b"*4\r\n$9\r\nXREVRANGE\r\n$2\r\nxs\r\n$3\r\n0-0\r\n$3\r\n0-0\r\n";
const XREVRANGE_ZERO_REPLY: &[u8] = b"*0\r\n";
const XPENDING_ZERO: &[u8] =
    b"*6\r\n$8\r\nXPENDING\r\n$2\r\nxs\r\n$1\r\ng\r\n$3\r\n0-0\r\n$3\r\n0-0\r\n$2\r\n10\r\n";
const XPENDING_ZERO_REPLY: &[u8] = b"*0\r\n";
const XREAD_AFTER_TAIL: &[u8] =
    b"*4\r\n$5\r\nXREAD\r\n$7\r\nSTREAMS\r\n$2\r\nxs\r\n$6\r\n1000-0\r\n";
const XREAD_AFTER_TAIL_REPLY: &[u8] = b"*-1\r\n";
const XREADGROUP_ALL_XS_G_C: &[u8] = b"*9\r\n$10\r\nXREADGROUP\r\n$5\r\nGROUP\r\n\
$1\r\ng\r\n$1\r\nc\r\n$5\r\nCOUNT\r\n$4\r\n1000\r\n$7\r\nSTREAMS\r\n$2\r\nxs\r\n\
$1\r\n>\r\n";
const XGROUP_CREATE_XS_G: &[u8] =
    b"*5\r\n$6\r\nXGROUP\r\n$6\r\nCREATE\r\n$2\r\nxs\r\n$1\r\ng\r\n$3\r\n0-0\r\n";
const XGROUP_CREATECONSUMER_XS_G_C: &[u8] = b"*5\r\n$6\r\nXGROUP\r\n$14\r\n\
CREATECONSUMER\r\n$2\r\nxs\r\n$1\r\ng\r\n$1\r\nc\r\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    MioA,
    MioB,
    IoUring,
    Redis,
}

impl Arm {
    const ALL: [Self; 4] = [Self::MioA, Self::MioB, Self::IoUring, Self::Redis];

    const fn index(self) -> usize {
        match self {
            Self::MioA => 0,
            Self::MioB => 1,
            Self::IoUring => 2,
            Self::Redis => 3,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::MioA => "mio_a",
            Self::MioB => "mio_b",
            Self::IoUring => "io_uring",
            Self::Redis => "redis",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    Set,
    Get,
    Mixed,
    MixedFamilies,
    BitposRange,
    BitfieldRoTwoGet,
    ObjectEncoding,
    ObjectRefcount,
    Dbsize,
    Echo,
    Unwatch,
    WaitZero,
    XtrimMinidNoop,
    XdelMissing,
    XackMissing,
    XclaimMissing,
    XrangeZero,
    XrevrangeZero,
    XpendingZero,
    XreadAfterTail,
    PttlPersistent,
    ZremrangebyscoreInverted,
    ZremrangebyrankNoop,
    ZremrangebylexNoop,
    ZrangebyscoreRange,
    ZrevrangebyscoreRange,
    ZrevrangeRankRange,
    ZrangebylexRange,
    ZrevrangebylexRange,
    LrangeInverted,
    LpopCountMissing,
    RpopCountMissing,
    LindexMiddle,
    LsetMiddleSameValue,
    LposMiddleElement,
    HdelMissingField,
    HgetMissingField,
    HexistsMissingField,
    HkeysFields,
    HvalsFields,
    HgetallFields,
    HscanAllFields,
    SscanCursorZero,
    HscanCursorZero,
    ZscanCursorZero,
    HsetSameValue,
    HsetnxExistingField,
    HincrbyZeroDelta,
    HincrbyfloatZeroDelta,
    HstrlenExistingField,
    HmgetExistingMissing,
    PfmergeTwoDense,
    PfcountTwoDense,
    BitcountOneMib,
    KeysSelectivePrefix,
    ScanSelectivePrefix,
    SunionstoreMixed,
    SdiffstoreMixed,
    SinterstoreMixed,
    ZintercardCached,
    ZdiffTwoKey,
    ZinterTwoKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandFloorAb {
    None,
    BitposRange,
    BitfieldRoTwoGet,
    ObjectEncoding,
    ObjectRefcount,
    Dbsize,
    Echo,
    WaitZero,
    XtrimMinidNoop,
    XtrimMinidNoopFloor,
    XdelMissingFloor,
    XackMissingFloor,
    XrangeZeroFloor,
    XrevrangeZeroFloor,
    LrangeFloor,
}

impl Workload {
    const fn name(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Get => "get",
            Self::Mixed => "mixed",
            Self::MixedFamilies => "mixed-families",
            Self::BitposRange => "bitpos-range",
            Self::BitfieldRoTwoGet => "bitfield-ro-two-get",
            Self::ObjectEncoding => "object-encoding",
            Self::ObjectRefcount => "object-refcount",
            Self::Dbsize => "dbsize",
            Self::Echo => "echo",
            Self::Unwatch => "unwatch",
            Self::WaitZero => "wait-zero",
            Self::XtrimMinidNoop => "xtrim-minid-noop",
            Self::XdelMissing => "xdel-missing",
            Self::XackMissing => "xack-missing",
            Self::XclaimMissing => "xclaim-missing",
            Self::XrangeZero => "xrange-zero",
            Self::XrevrangeZero => "xrevrange-zero",
            Self::XpendingZero => "xpending-zero",
            Self::XreadAfterTail => "xread-after-tail",
            Self::PttlPersistent => "pttl-persistent",
            Self::ZremrangebyscoreInverted => "zremrangebyscore-inverted",
            Self::ZremrangebyrankNoop => "zremrangebyrank-noop",
            Self::ZremrangebylexNoop => "zremrangebylex-noop",
            Self::ZrangebyscoreRange => "zrangebyscore-range",
            Self::ZrevrangebyscoreRange => "zrevrangebyscore-range",
            Self::ZrevrangeRankRange => "zrevrange-rank-range",
            Self::ZrangebylexRange => "zrangebylex-range",
            Self::ZrevrangebylexRange => "zrevrangebylex-range",
            Self::LrangeInverted => "lrange-inverted",
            Self::LpopCountMissing => "lpop-count-missing",
            Self::RpopCountMissing => "rpop-count-missing",
            Self::LindexMiddle => "lindex-middle",
            Self::LsetMiddleSameValue => "lset-middle-same-value",
            Self::LposMiddleElement => "lpos-middle-element",
            Self::HdelMissingField => "hdel-missing-field",
            Self::HgetMissingField => "hget-missing-field",
            Self::HexistsMissingField => "hexists-missing-field",
            Self::HkeysFields => "hkeys-fields",
            Self::HvalsFields => "hvals-fields",
            Self::HgetallFields => "hgetall-fields",
            Self::HscanAllFields => "hscan-all-fields",
            Self::SscanCursorZero => "sscan-cursor-zero",
            Self::HscanCursorZero => "hscan-cursor-zero",
            Self::ZscanCursorZero => "zscan-cursor-zero",
            Self::HsetSameValue => "hset-same-value",
            Self::HsetnxExistingField => "hsetnx-existing-field",
            Self::HincrbyZeroDelta => "hincrby-zero-delta",
            Self::HincrbyfloatZeroDelta => "hincrbyfloat-zero-delta",
            Self::HstrlenExistingField => "hstrlen-existing-field",
            Self::HmgetExistingMissing => "hmget-existing-missing",
            Self::PfmergeTwoDense => "pfmerge-two-dense",
            Self::PfcountTwoDense => "pfcount-two-dense",
            Self::BitcountOneMib => "bitcount-one-mib",
            Self::KeysSelectivePrefix => "keys-selective-prefix",
            Self::ScanSelectivePrefix => "scan-selective-prefix",
            Self::SunionstoreMixed => "sunionstore-mixed",
            Self::SdiffstoreMixed => "sdiffstore-mixed",
            Self::SinterstoreMixed => "sinterstore-mixed",
            Self::ZintercardCached => "zintercard-cached",
            Self::ZdiffTwoKey => "zdiff-two-key",
            Self::ZinterTwoKey => "zinter-two-key",
        }
    }

    const fn profile_targets(self) -> &'static [&'static str] {
        match self {
            Self::BitposRange => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_bitpos_borrowed",
                "bitpos_impl",
                "bitpos_full_bytes",
                "parse_borrowed_plain_bitpos_range_packet",
            ],
            Self::BitfieldRoTwoGet => &[
                "frankenredis::process_buffered_frames",
                "fr_command::bitfield_ro_cmd",
                "bitfield_get_batch",
                "parse_command_args_borrowed_into",
                "copy_borrowed_argv_into_scratch",
            ],
            Self::ObjectEncoding => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_object_encoding_borrowed_into",
                "parse_borrowed_plain_object_encoding_packet",
                "object_encoding",
            ],
            Self::ObjectRefcount => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_object_refcount_borrowed",
                "parse_borrowed_plain_object_refcount_packet",
                "object_refcount",
            ],
            Self::Dbsize => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_dbsize",
                "execute_plain_dbsize_borrowed",
                "parse_borrowed_plain_dbsize_packet",
                "dbsize_in_db",
            ],
            Self::Echo => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_echo_into",
                "execute_plain_echo_borrowed_into",
                "parse_borrowed_plain_echo_packet",
            ],
            Self::Unwatch => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_unwatch_borrowed_into",
                "parse_borrowed_plain_unwatch_packet",
            ],
            Self::WaitZero => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_wait_zero",
                "execute_plain_wait_borrowed",
                "parse_borrowed_plain_key_arg1_packet",
            ],
            Self::XtrimMinidNoop => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_xtrim_minid_noop",
                "execute_plain_xtrim_minid_noop_borrowed",
                "fr_command::xtrim",
                "fr_store::Store::xtrim_minid_approx",
                "xtrim_minid_noop_guard_enabled",
            ],
            Self::XdelMissing => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_xdel_missing",
                "execute_plain_xdel_missing_borrowed",
                "parse_borrowed_plain_key_arg1_packet",
                "fr_runtime::Runtime::execute_internal",
                "fr_command::execute_dispatch",
                "fr_command::xdel",
                "fr_store::Store::xdel",
            ],
            Self::XackMissing => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_xack_missing",
                "execute_plain_xack_missing_borrowed",
                "parse_borrowed_plain_key_arg2_packet",
                "fr_runtime::Runtime::execute_internal",
                "fr_command::execute_dispatch",
                "fr_command::xack_cmd",
                "fr_store::Store::stream_group_exists",
                "fr_store::Store::xack",
                "fr_store::Store::invalidate_stream_pel_summary",
            ],
            Self::XclaimMissing => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_xclaim_missing",
                "execute_plain_xclaim_missing_borrowed",
                "parse_borrowed_plain_key_arg4_packet",
                "fr_runtime::Runtime::execute_internal",
                "fr_command::execute_dispatch",
                "fr_command::xclaim",
                "fr_store::Store::xclaim",
                "fr_store::StreamGroup::insert_consumer",
            ],
            Self::XrangeZero => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_xrange_zero",
                "execute_plain_xrange_zero_borrowed_into",
                "execute_plain_xrange_borrowed_into",
                "parse_borrowed_plain_key_arg2_packet",
                "fr_command::parse_stream_range_bound",
                "fr_store::Store::xrange_borrow_scan",
            ],
            Self::XrevrangeZero => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_xrevrange_zero",
                "execute_plain_xrevrange_zero_borrowed_into",
                "execute_plain_xrevrange_borrowed_into",
                "parse_borrowed_plain_key_arg2_packet",
                "fr_command::parse_stream_range_bound",
                "fr_store::Store::xrange_borrow_scan",
            ],
            Self::XpendingZero => &[
                "frankenredis::process_buffered_frames",
                "dispatch_floor_fast_xpending_zero",
                "execute_plain_xpending_zero_borrowed_into",
                "parse_borrowed_plain_key_arg4_packet",
                "fr_runtime::Runtime::execute_internal",
                "fr_command::execute_dispatch",
                "fr_command::xpending",
                "fr_command::parse_stream_range_bound",
                "fr_store::Store::xpending_entries",
            ],
            Self::XreadAfterTail => &[
                "frankenredis::process_buffered_frames",
                "execute_plain_xread_single_borrowed_into",
                "parse_borrowed_plain_xread_single_packet",
                "fr_command::parse_stream_range_bound",
                "fr_store::Store::xlast_id",
                "fr_store::Store::xread_borrow_scan",
            ],
            Self::PttlPersistent => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "execute_plain_keymeta_borrowed",
                "parse_borrowed_plain_pttl_packet",
                "fr_store::Store::pttl",
                "fr_store::Store::get_expires_at_ms",
            ],
            Self::ZremrangebyscoreInverted => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "execute_plain_zremrangebyscore_borrowed",
                "parse_borrowed_plain_key_arg2_packet",
                "fr_command::parse_score_bound",
                "fr_store::Store::zremrangebyscore",
                "fr_store::SortedSet::score_bound_range",
            ],
            Self::ZremrangebyrankNoop => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "execute_plain_zremrangebyrank_borrowed",
                "parse_borrowed_plain_key_arg2_packet",
                "fr_store::Store::zremrangebyrank",
                "fr_store::normalize_index",
            ],
            Self::ZremrangebylexNoop => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "execute_plain_zremrangebylex_borrowed",
                "parse_borrowed_plain_key_arg2_packet",
                "fr_store::Store::zremrangebylex",
                "fr_store::SortedSet::lex_range_asc",
            ],
            Self::ZrangebyscoreRange => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg2_packet",
                "execute_plain_zrangebyscore_borrowed_into",
                "execute_plain_zrangebyscore_core_into",
                "fr_store::Store::zrangebyscore_members_borrow_scan",
                "fr_store::SortedSet::score_bound_range_asc_refs",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::ZrevrangebyscoreRange => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg2_packet",
                "execute_plain_zrevrangebyscore_borrowed_into",
                "execute_plain_zrangebyscore_core_into",
                "fr_store::Store::zrangebyscore_members_borrow_scan",
                "fr_store::SortedSet::score_bound_range_desc_refs",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::ZrevrangeRankRange => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg2_packet",
                "execute_plain_zrevrange_borrowed_into",
                "fr_store::Store::zrevrange_borrow_scan",
                "fr_store::Store::zrevrange_borrow_scan_impl",
                "fr_store::normalize_index",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::ZrangebylexRange => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_zrangebylex_packet",
                "execute_plain_zrangebylex_borrowed_into",
                "fr_store::Store::zrangebylex_members_borrow_scan",
                "fr_store::SortedSet::range_by_lex",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::ZrevrangebylexRange => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_zrevrangebylex_packet",
                "execute_plain_zrevrangebylex_borrowed_into",
                "fr_store::Store::zrangebylex_members_borrow_scan",
                "fr_store::SortedSet::range_by_lex",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::LrangeInverted => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "dispatch_floor_fast_lrange_into",
                "parse_borrowed_plain_lrange_packet",
                "execute_plain_lrange_borrowed_into",
                "fr_store::Store::lrange_borrow_scan",
                "fr_store::normalize_index",
            ],
            Self::LpopCountMissing => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg1_packet",
                "execute_plain_list_pop_count_borrowed",
                "fr_store::Store::lpop_count",
                "fr_store::Store::lpop_count_impl",
            ],
            Self::RpopCountMissing => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg1_packet",
                "execute_plain_list_pop_count_borrowed",
                "fr_store::Store::rpop_count",
                "fr_store::Store::rpop_count_impl",
            ],
            Self::LindexMiddle => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_lindex_packet",
                "execute_plain_lindex_borrowed_into",
                "fr_store::Store::lindex_with",
                "fr_store::Store::lindex_with_impl",
                "fr_store::packed_set::ListValue::get",
                "fr_store::packed_set::ChunkedList::get",
                "fr_store::packed_set::ChunkedList::locate",
                "fr_store::packed_set::ListChunk::get",
            ],
            Self::LsetMiddleSameValue => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_lset_packet",
                "fr_runtime::Runtime::execute_plain_lset_borrowed",
                "fr_store::Store::lset",
                "fr_store::packed_set::ListValue::set",
                "fr_store::packed_set::ChunkedList::set",
                "fr_store::packed_set::ListChunk::set",
            ],
            Self::LposMiddleElement => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "frankenredis::dispatch_floor_fast_lpos",
                "parse_borrowed_plain_lpos_packet",
                "fr_runtime::Runtime::execute_plain_lpos_borrowed",
                "fr_store::Store::lpos_full",
                "fr_store::packed_set::ListValue::iter",
                "fr_store::packed_set::ChunkedList::iter",
                "fr_store::packed_set::ListValueIter::next",
                "fr_store::packed_set::ChunkedListIter::next",
                "fr_store::packed_set::ListChunkIter::next",
            ],
            Self::HdelMissingField => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_keyed_values1_packet",
                "execute_plain_keyed_values_write_borrowed",
                "fr_store::Store::hdel",
                "fr_store::Store::hdel_impl",
                "fr_store::Store::hdel_apply",
                "fr_store::packed_set::CompactFieldMap::delete",
            ],
            Self::HgetMissingField => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hget_packet",
                "execute_plain_hget_borrowed_into",
                "fr_store::Store::hget_with",
                "fr_store::Store::lookup_live_for_read_mut",
                "fr_store::packed_set::HashFieldMap::get",
                "fr_store::packed_set::CompactFieldMap::get",
                "fr_store::packed_set::CompactFieldMap::lookup_slot",
                "fr_store::packed_set::CompactFieldMap::lookup_slot_prehashed",
            ],
            Self::HexistsMissingField => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hexists_packet",
                "execute_plain_hexists_borrowed",
                "fr_store::Store::hexists",
                "fr_store::Store::lookup_live_for_read_mut",
                "fr_store::packed_set::HashFieldMap::contains_key",
                "fr_store::packed_set::CompactFieldMap::contains_key",
                "fr_store::packed_set::CompactFieldMap::lookup_slot",
                "fr_store::packed_set::CompactFieldMap::lookup_slot_prehashed",
            ],
            Self::HkeysFields => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hkeys_packet",
                "fr_runtime::Runtime::execute_plain_hcoll_borrowed_into",
                "fr_store::Store::hcollection_borrow_scan",
                "fr_store::Store::hcollection_borrow_scan_impl",
                "fr_store::packed_set::HashFieldMap::keys",
                "fr_store::packed_set::HashFieldMapKeyIter::next",
                "fr_store::packed_set::CompactFieldMapFieldIter::next",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::HvalsFields => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hvals_packet",
                "fr_runtime::Runtime::execute_plain_hcoll_borrowed_into",
                "fr_store::Store::hcollection_borrow_scan",
                "fr_store::Store::hcollection_borrow_scan_impl",
                "fr_store::packed_set::HashFieldMap::values",
                "fr_store::packed_set::HashFieldMapIter::next",
                "fr_store::packed_set::CompactFieldMapIter::next",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::HgetallFields => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hgetall_packet",
                "fr_runtime::Runtime::execute_plain_hgetall_borrowed_into",
                "fr_store::Store::hgetall_borrow_scan",
                "fr_store::Store::hgetall_borrow_scan_impl",
                "fr_store::packed_set::HashFieldMapIter::next",
                "fr_store::packed_set::CompactFieldMapIter::next",
                "fr_protocol::encode_map_header",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::HscanAllFields => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_multibulk_action",
                "parse_command_args_borrowed_into",
                "copy_borrowed_argv_into_scratch",
                "fr_runtime::Runtime::execute_frame_internal",
                "fr_command::execute_dispatch",
                "fr_command::hscan",
                "fr_store::Store::hscan",
                "fr_store::packed_set::HashFieldMap::get_index",
                "fr_store::packed_set::CompactFieldMap::get_index",
                "<fr_protocol::RespFrame>::encode_into",
            ],
            Self::SscanCursorZero => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg1_packet",
                "execute_plain_sscan0_borrowed_into",
                "fr_store::Store::sscan0_borrow_scan",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::HscanCursorZero => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg1_packet",
                "execute_plain_hscan0_borrowed_into",
                "fr_store::Store::hscan0_borrow_scan",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::ZscanCursorZero => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg1_packet",
                "execute_plain_zscan0_borrowed_into",
                "fr_store::Store::zscan0_borrow_scan",
                "fr_protocol::encode_bulk_string_slice",
            ],
            Self::HsetSameValue => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hset_packet",
                "fr_runtime::Runtime::execute_plain_hset_borrowed",
                "fr_runtime::Runtime::execute_plain_hset_borrowed_with_default_write_gate",
                "fr_store::Store::hset_borrowed",
                "fr_store::Store::hset_borrowed_impl",
                "fr_store::packed_set::HashFieldMap::insert_borrowed",
                "fr_store::packed_set::CompactFieldMap::insert_borrowed",
                "fr_store::packed_set::CompactFieldMap::lookup_slot",
                "fr_store::packed_set::CompactFieldMap::lookup_slot_prehashed",
            ],
            Self::HsetnxExistingField => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hsetnx_packet",
                "execute_plain_hsetnx_borrowed",
                "fr_store::Store::hsetnx",
                "fr_store::Store::hsetnx_impl",
                "fr_store::packed_set::HashFieldMap::contains_key",
                "fr_store::packed_set::CompactFieldMap::contains_key",
                "fr_store::packed_set::CompactFieldMap::lookup_slot",
                "fr_store::packed_set::CompactFieldMap::lookup_slot_prehashed",
            ],
            Self::HincrbyZeroDelta => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hincrby_packet",
                "fr_runtime::Runtime::execute_plain_hincrby_borrowed",
                "fr_store::Store::hincrby",
                "fr_store::Store::hincrby_impl",
                "fr_store::Store::with_mutated_or_created_entry",
                "fr_store::packed_set::HashFieldMap::get",
                "fr_store::packed_set::HashFieldMap::insert_borrowed",
                "fr_store::packed_set::CompactFieldMap::get",
                "fr_store::packed_set::CompactFieldMap::insert_borrowed",
                "fr_store::packed_set::CompactFieldMap::lookup_slot",
                "fr_store::packed_set::CompactFieldMap::lookup_slot_prehashed",
            ],
            Self::HincrbyfloatZeroDelta => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hincrbyfloat_packet",
                "fr_runtime::Runtime::execute_plain_hincrbyfloat_borrowed",
                "fr_store::Store::hincrbyfloat_text",
                "fr_store::Store::hincrbyfloat_text_impl",
                "fr_store::Store::with_mutated_or_created_entry",
                "fr_store::packed_set::HashFieldMap::get",
                "fr_store::packed_set::HashFieldMap::insert_borrowed",
                "fr_store::packed_set::CompactFieldMap::get",
                "fr_store::packed_set::CompactFieldMap::insert_borrowed",
                "fr_store::packed_set::CompactFieldMap::lookup_slot",
                "fr_store::packed_set::CompactFieldMap::lookup_slot_prehashed",
            ],
            Self::HstrlenExistingField => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hstrlen_packet",
                "execute_plain_hstrlen_borrowed",
                "fr_store::Store::hstrlen",
                "fr_store::Store::lookup_live_for_read_mut",
                "fr_store::packed_set::HashFieldMap::get",
                "fr_store::packed_set::CompactFieldMap::get",
                "fr_store::packed_set::CompactFieldMap::lookup_slot",
                "fr_store::packed_set::CompactFieldMap::lookup_slot_prehashed",
            ],
            Self::HmgetExistingMissing => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_hmget2_packet",
                "execute_plain_hmget_borrowed_into",
                "fr_store::Store::hmget_for_each",
                "fr_store::packed_set::HashFieldMap::get",
                "fr_store::packed_set::CompactFieldMap::get",
                "fr_store::packed_set::CompactFieldMap::lookup_slot",
                "fr_store::packed_set::CompactFieldMap::lookup_slot_prehashed",
            ],
            Self::PfmergeTwoDense => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_multibulk_action",
                "parse_command_args_borrowed_into",
                "copy_borrowed_argv_into_scratch",
                "fr_runtime::Runtime::execute_frame_internal",
                "fr_command::execute_dispatch",
                "fr_command::pfmerge",
                "fr_store::Store::pfmerge",
                "fr_store::hll_parse",
                "fr_store::hll_decode_dense_registers",
                "fr_store::hll_merge_fold",
                "fr_store::hll_merge_registers",
                "fr_simd::max_bytes_inplace",
                "fr_store::hll_encode",
                "fr_store::hll_encode_dense_registers",
            ],
            Self::PfcountTwoDense => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_multibulk_action",
                "parse_command_args_borrowed_into",
                "copy_borrowed_argv_into_scratch",
                "fr_runtime::Runtime::execute_frame_internal",
                "fr_command::execute_dispatch",
                "fr_command::pfcount",
                "fr_store::Store::pfcount",
                "fr_store::hll_parse",
                "fr_store::hll_merge_fold",
                "fr_store::hll_merge_registers",
                "fr_simd::max_bytes_inplace",
                "fr_store::hll_estimate",
            ],
            Self::BitcountOneMib => &[
                "frankenredis::process_buffered_frames",
                "parse_borrowed_plain_bitcount_packet",
                "fr_runtime::Runtime::execute_plain_bitcount_borrowed",
                "fr_store::Store::bitcount",
                "fr_store::Store::bitcount_impl",
                "fr_store::Store::popcount_bytes",
                "fr_simd::popcount_bytes",
                "fr_simd::popcount_avx2",
                "fr_simd::popcount_popcnt",
                "fr_simd::popcount_scalar",
            ],
            Self::KeysSelectivePrefix => &[
                "<fr_runtime::Runtime>::handle_db_keys_command",
                "<fr_store::Store>::keys_matching_in_db",
                "fr_store::glob_literal_prefix",
                "<fr_store::PreparedGlob>::matches",
            ],
            Self::ScanSelectivePrefix => &[
                "<fr_runtime::Runtime>::handle_scan_command",
                "<fr_store::Store>::scan_in_db",
                "fr_store::glob_literal_prefix",
                "<fr_store::PreparedGlob>::matches",
            ],
            Self::SunionstoreMixed => &[
                "frankenredis::process_buffered_frames",
                "parse_borrowed_plain_key_arg2_packet",
                "<fr_runtime::Runtime>::execute_plain_sunionstore_borrowed",
                "<fr_runtime::Runtime>::execute_plain_setstore_borrowed",
                "<fr_store::Store>::sunionstore",
                "<fr_store::Store>::sunion_value",
                "<fr_store::SetValue>::union_with",
                "<fr_store::SetValue>::insert_borrowed",
                "<fr_store::Store>::store_set_algebra_value",
                "<fr_store::Store>::set_value_entry",
                "<fr_store::SetValue>::from_index_set",
                "<fr_store::packed_set::GenericSet>::insert_borrowed",
                "<fr_store::packed_set::GenericSet>::shrink_to_fit",
                "<fr_store::packed_set::CompactStrSet>::shrink_to_fit",
                "<fr_store::packed_set::CompactFieldMap>::insert",
                "<fr_store::packed_set::CompactFieldMap>::lookup_slot_prehashed",
                "<fr_store::packed_set::CompactFieldMap>::append_entry",
                "<fr_store::packed_set::CompactFieldMap>::maybe_compact",
                "<fr_store::packed_set::CompactFieldMap>::rehash",
                "<fr_store::Store>::internal_entries_insert",
                "fr_store::integer_decimal_bytes",
            ],
            Self::SdiffstoreMixed => &[
                "frankenredis::process_buffered_frames",
                "parse_borrowed_plain_key_arg2_packet",
                "<fr_runtime::Runtime>::execute_plain_sdiffstore_borrowed",
                "<fr_runtime::Runtime>::execute_plain_setstore_borrowed",
                "<fr_store::Store>::sdiffstore",
                "<fr_store::Store>::sdiff_value",
                "<fr_store::SetValue>::retain_diff",
                "<fr_store::SetValue>::retain",
                "<fr_store::SetValue>::contains",
                "<fr_store::packed_set::GenericSet>::contains",
                "<fr_store::packed_set::CompactStrSet>::contains",
                "<fr_store::packed_set::CompactFieldMap>::contains_key",
                "<fr_store::packed_set::CompactFieldMap>::lookup_slot_prehashed",
                "<fr_store::Store>::store_set_algebra_value",
                "<fr_store::Store>::set_value_entry",
                "<fr_store::Store>::internal_entries_insert",
                "fr_store::set_int_to_bytes",
            ],
            Self::SinterstoreMixed => &[
                "frankenredis::process_buffered_frames",
                "parse_borrowed_plain_key_arg2_packet",
                "<fr_runtime::Runtime>::execute_plain_sinterstore_borrowed",
                "<fr_runtime::Runtime>::execute_plain_setstore_borrowed",
                "<fr_store::Store>::sinterstore",
                "<fr_store::Store>::sinter_prepare",
                "<fr_store::Store>::sinter_value",
                "<fr_store::SetValue>::retain_intersect",
                "<fr_store::SetValue>::retain",
                "<fr_store::SetValue>::contains",
                "<fr_store::packed_set::GenericSet>::contains",
                "<fr_store::packed_set::CompactStrSet>::contains",
                "<fr_store::packed_set::CompactFieldMap>::contains_key",
                "<fr_store::packed_set::CompactFieldMap>::lookup_slot_prehashed",
                "<fr_store::Store>::store_set_algebra_value",
                "<fr_store::Store>::set_value_entry",
                "<fr_store::Store>::internal_entries_insert",
                "fr_store::integer_decimal_bytes",
                "fr_store::set_int_to_bytes",
            ],
            Self::ZintercardCached => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_multibulk_action",
                "parse_command_args_borrowed_into",
                "copy_borrowed_argv_into_scratch",
                "fr_runtime::Runtime::execute_frame_internal",
                "fr_command::execute_dispatch",
                "fr_command::zintercard",
                "fr_command::record_source_key_lookups",
                "<fr_store::Store>::peek_value_type",
                "<fr_store::Store>::zintercard_count_cached",
                "<fr_store::Store>::zintercard_cache_hit",
            ],
            Self::ZdiffTwoKey => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg2_packet",
                "fr_runtime::Runtime::execute_plain_zdiff2_borrowed",
                "fr_runtime::Runtime::execute_plain_zdiff2_core",
                "fr_store::Store::zdiff_members_no_stats",
                "fr_protocol::encode",
            ],
            Self::ZinterTwoKey => &[
                "frankenredis::process_buffered_frames",
                "__memcmp_avx2_movbe",
                "parse_borrowed_plain_key_arg2_packet",
                "fr_runtime::Runtime::execute_plain_zinter_borrowed",
                "fr_runtime::Runtime::execute_plain_zinter_core",
                "fr_store::Store::zinter_members_argv_order_no_stats",
                "fr_protocol::encode",
            ],
            Self::Set | Self::Get | Self::Mixed | Self::MixedFamilies => &[],
        }
    }

    fn parse_list() -> Vec<Self> {
        let value = std::env::var("FR_URING_AB_WORKLOADS").unwrap_or_else(|_| "set".to_owned());
        value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| match item {
                "set" => Self::Set,
                "get" => Self::Get,
                "mixed" => Self::Mixed,
                "mixed-families" => Self::MixedFamilies,
                "bitpos-range" => Self::BitposRange,
                "bitfield-ro-two-get" => Self::BitfieldRoTwoGet,
                "object-encoding" => Self::ObjectEncoding,
                "object-refcount" => Self::ObjectRefcount,
                "dbsize" => Self::Dbsize,
                "echo" => Self::Echo,
                "unwatch" => Self::Unwatch,
                "wait-zero" => Self::WaitZero,
                "xtrim-minid-noop" => Self::XtrimMinidNoop,
                "xdel-missing" => Self::XdelMissing,
                "xack-missing" => Self::XackMissing,
                "xclaim-missing" => Self::XclaimMissing,
                "xrange-zero" => Self::XrangeZero,
                "xrevrange-zero" => Self::XrevrangeZero,
                "xpending-zero" => Self::XpendingZero,
                "xread-after-tail" => Self::XreadAfterTail,
                "pttl-persistent" => Self::PttlPersistent,
                "zremrangebyscore-inverted" => Self::ZremrangebyscoreInverted,
                "zremrangebyrank-noop" => Self::ZremrangebyrankNoop,
                "zremrangebylex-noop" => Self::ZremrangebylexNoop,
                "zrangebyscore-range" => Self::ZrangebyscoreRange,
                "zrevrangebyscore-range" => Self::ZrevrangebyscoreRange,
                "zrevrange-rank-range" => Self::ZrevrangeRankRange,
                "zrangebylex-range" => Self::ZrangebylexRange,
                "zrevrangebylex-range" => Self::ZrevrangebylexRange,
                "lrange-inverted" => Self::LrangeInverted,
                "lpop-count-missing" => Self::LpopCountMissing,
                "rpop-count-missing" => Self::RpopCountMissing,
                "lindex-middle" => Self::LindexMiddle,
                "lset-middle-same-value" => Self::LsetMiddleSameValue,
                "lpos-middle-element" => Self::LposMiddleElement,
                "hdel-missing-field" => Self::HdelMissingField,
                "hget-missing-field" => Self::HgetMissingField,
                "hexists-missing-field" => Self::HexistsMissingField,
                "hkeys-fields" => Self::HkeysFields,
                "hvals-fields" => Self::HvalsFields,
                "hgetall-fields" => Self::HgetallFields,
                "hscan-all-fields" => Self::HscanAllFields,
                "sscan-cursor-zero" => Self::SscanCursorZero,
                "hscan-cursor-zero" => Self::HscanCursorZero,
                "zscan-cursor-zero" => Self::ZscanCursorZero,
                "hset-same-value" => Self::HsetSameValue,
                "hsetnx-existing-field" => Self::HsetnxExistingField,
                "hincrby-zero-delta" => Self::HincrbyZeroDelta,
                "hincrbyfloat-zero-delta" => Self::HincrbyfloatZeroDelta,
                "hstrlen-existing-field" => Self::HstrlenExistingField,
                "hmget-existing-missing" => Self::HmgetExistingMissing,
                "pfmerge-two-dense" => Self::PfmergeTwoDense,
                "pfcount-two-dense" => Self::PfcountTwoDense,
                "bitcount-one-mib" => Self::BitcountOneMib,
                "keys-selective-prefix" => Self::KeysSelectivePrefix,
                "scan-selective-prefix" => Self::ScanSelectivePrefix,
                "sunionstore-mixed" => Self::SunionstoreMixed,
                "sdiffstore-mixed" => Self::SdiffstoreMixed,
                "sinterstore-mixed" => Self::SinterstoreMixed,
                "zintercard-cached" => Self::ZintercardCached,
                "zdiff-two-key" => Self::ZdiffTwoKey,
                "zinter-two-key" => Self::ZinterTwoKey,
                other => panic!("unknown FR_URING_AB_WORKLOADS item: {other}"),
            })
            .collect()
    }
}

struct ExchangeCase {
    request: Vec<u8>,
    response: Vec<u8>,
}

struct WorkloadPackets {
    even: ExchangeCase,
    odd: ExchangeCase,
}

impl WorkloadPackets {
    fn new(workload: Workload, pipeline: usize) -> Self {
        assert!(pipeline > 0, "pipeline depth must be positive");
        match workload {
            Workload::Set => {
                let case = repeated_case(SET, SET_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Get => {
                let case = repeated_case(GET, GET_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Mixed => Self {
                even: mixed_case(pipeline, false),
                odd: mixed_case(pipeline, true),
            },
            Workload::MixedFamilies => {
                panic!("mixed-families requires per-connection keyed packets")
            }
            Workload::BitposRange => {
                let case = repeated_case(BITPOS_RANGE, BITPOS_RANGE_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::BitfieldRoTwoGet => {
                let case = repeated_case(BITFIELD_RO_TWO_GET, BITFIELD_RO_TWO_GET_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ObjectEncoding => {
                let case = repeated_case(OBJECT_ENCODING, OBJECT_ENCODING_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ObjectRefcount => {
                let case = repeated_case(OBJECT_REFCOUNT, OBJECT_REFCOUNT_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Dbsize => {
                let case = repeated_case(DBSIZE, DBSIZE_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Echo => {
                let case = repeated_case(ECHO, ECHO_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::Unwatch => {
                let case = repeated_case(UNWATCH, UNWATCH_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::WaitZero => {
                let case = repeated_case(WAIT_ZERO, WAIT_ZERO_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XtrimMinidNoop => {
                let case = repeated_case(XTRIM_MINID_NOOP, XTRIM_MINID_NOOP_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XdelMissing => {
                let case = repeated_case(XDEL_MISSING, XDEL_MISSING_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XackMissing => {
                let case = repeated_case(XACK_MISSING, XACK_MISSING_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XclaimMissing => {
                let case = repeated_case(XCLAIM_MISSING, XCLAIM_MISSING_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XrangeZero => {
                let case = repeated_case(XRANGE_ZERO, XRANGE_ZERO_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XrevrangeZero => {
                let case = repeated_case(XREVRANGE_ZERO, XREVRANGE_ZERO_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XpendingZero => {
                let case = repeated_case(XPENDING_ZERO, XPENDING_ZERO_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::XreadAfterTail => {
                let case = repeated_case(XREAD_AFTER_TAIL, XREAD_AFTER_TAIL_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::PttlPersistent => {
                let case = repeated_case(PTTL_PERSISTENT, PTTL_PERSISTENT_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZremrangebyscoreInverted => {
                let case = repeated_case(
                    ZREMRANGEBYSCORE_INVERTED,
                    ZREMRANGEBYSCORE_INVERTED_REPLY,
                    pipeline,
                );
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZremrangebyrankNoop => {
                let case =
                    repeated_case(ZREMRANGEBYRANK_NOOP, ZREMRANGEBYRANK_NOOP_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZremrangebylexNoop => {
                let case = repeated_case(ZREMRANGEBYLEX_NOOP, ZREMRANGEBYLEX_NOOP_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZrangebyscoreRange => {
                let response = zrangebyscore_range_reply();
                let case = repeated_case(ZRANGEBYSCORE_RANGE, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZrevrangebyscoreRange => {
                let response = zrevrangebyscore_range_reply();
                let case = repeated_case(ZREVRANGEBYSCORE_RANGE, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZrevrangeRankRange => {
                let response = zrevrange_rank_range_reply();
                let case = repeated_case(ZREVRANGE_RANK_RANGE, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZrangebylexRange => {
                let response = zrangebylex_range_reply();
                let case = repeated_case(ZRANGEBYLEX_RANGE, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZrevrangebylexRange => {
                let response = zrevrangebylex_range_reply();
                let case = repeated_case(ZREVRANGEBYLEX_RANGE, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::LrangeInverted => {
                let case = repeated_case(LRANGE_INVERTED, LRANGE_INVERTED_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::LpopCountMissing => {
                let case = repeated_case(LPOP_COUNT_MISSING, LIST_COUNT_MISSING_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::RpopCountMissing => {
                let case = repeated_case(RPOP_COUNT_MISSING, LIST_COUNT_MISSING_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::LindexMiddle => {
                let case = repeated_case(LINDEX_MIDDLE, LINDEX_MIDDLE_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::LsetMiddleSameValue => {
                let case = repeated_case(
                    LSET_MIDDLE_SAME_VALUE,
                    LSET_MIDDLE_SAME_VALUE_REPLY,
                    pipeline,
                );
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::LposMiddleElement => {
                let case = repeated_case(LPOS_MIDDLE_ELEMENT, LPOS_MIDDLE_ELEMENT_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HdelMissingField => {
                let case = repeated_case(HDEL_MISSING_FIELD, HDEL_MISSING_FIELD_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HgetMissingField => {
                let case = repeated_case(HGET_MISSING_FIELD, HGET_MISSING_FIELD_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HexistsMissingField => {
                let case =
                    repeated_case(HEXISTS_MISSING_FIELD, HEXISTS_MISSING_FIELD_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HkeysFields => {
                let response = hkeys_fields_reply();
                let case = repeated_case(HKEYS_FIELDS, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HvalsFields => {
                let response = hvals_fields_reply();
                let case = repeated_case(HVALS_FIELDS, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HgetallFields => {
                let response = hgetall_fields_reply();
                let case = repeated_case(HGETALL_FIELDS, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HscanAllFields => {
                let response = hscan_all_fields_reply();
                let case = repeated_case(HSCAN_ALL_FIELDS, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::SscanCursorZero | Workload::HscanCursorZero | Workload::ZscanCursorZero => {
                let (request, response) = scan0_request_reply(workload);
                let case = repeated_case(request, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HsetSameValue => {
                let case = repeated_case(HSET_SAME_VALUE, HSET_SAME_VALUE_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HsetnxExistingField => {
                let case =
                    repeated_case(HSETNX_EXISTING_FIELD, HSETNX_EXISTING_FIELD_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HincrbyZeroDelta => {
                let case = repeated_case(HINCRBY_ZERO_DELTA, HINCRBY_ZERO_DELTA_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HincrbyfloatZeroDelta => {
                let case = repeated_case(
                    HINCRBYFLOAT_ZERO_DELTA,
                    HINCRBYFLOAT_ZERO_DELTA_REPLY,
                    pipeline,
                );
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HstrlenExistingField => {
                let case = repeated_case(
                    HSTRLEN_EXISTING_FIELD,
                    HSTRLEN_EXISTING_FIELD_REPLY,
                    pipeline,
                );
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::HmgetExistingMissing => {
                let case = repeated_case(
                    HMGET_EXISTING_MISSING,
                    HMGET_EXISTING_MISSING_REPLY,
                    pipeline,
                );
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::PfmergeTwoDense => {
                let case = repeated_case(PFMERGE_TWO_DENSE, PFMERGE_TWO_DENSE_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::PfcountTwoDense => {
                let case = repeated_case(PFCOUNT_TWO_DENSE, PFCOUNT_TWO_DENSE_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::BitcountOneMib => {
                let case = repeated_case(BITCOUNT_ONE_MIB, BITCOUNT_ONE_MIB_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::KeysSelectivePrefix => {
                let case =
                    repeated_case(KEYS_SELECTIVE_PREFIX, KEYS_SELECTIVE_PREFIX_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ScanSelectivePrefix => {
                let case =
                    repeated_case(SCAN_SELECTIVE_PREFIX, SCAN_SELECTIVE_PREFIX_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::SunionstoreMixed => {
                let case = repeated_case(SUNIONSTORE_MIXED, SUNIONSTORE_MIXED_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::SdiffstoreMixed => {
                let case = repeated_case(SDIFFSTORE_MIXED, SDIFFSTORE_MIXED_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::SinterstoreMixed => {
                let case = repeated_case(SINTERSTORE_MIXED, SINTERSTORE_MIXED_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZintercardCached => {
                let case = repeated_case(ZINTERCARD_CACHED, ZINTERCARD_CACHED_REPLY, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZdiffTwoKey => {
                let response = zdiff_two_key_reply();
                let case = repeated_case(ZDIFF_TWO_KEY, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
            Workload::ZinterTwoKey => {
                let response = zinter_two_key_reply();
                let case = repeated_case(ZINTER_TWO_KEY, &response, pipeline);
                Self {
                    odd: ExchangeCase {
                        request: case.request.clone(),
                        response: case.response.clone(),
                    },
                    even: case,
                }
            }
        }
    }
}

struct ConnectionPackets {
    measured: WorkloadPackets,
    setup: Option<ExchangeCase>,
}

struct DriverPackets {
    connections: Vec<ConnectionPackets>,
}

impl DriverPackets {
    fn shared(workload: Workload, pipeline: usize) -> Self {
        Self {
            connections: vec![ConnectionPackets {
                measured: WorkloadPackets::new(workload, pipeline),
                setup: None,
            }],
        }
    }

    fn keyed(workload: Workload, pipeline: usize, keys: &[Vec<u8>]) -> Self {
        assert!(
            matches!(workload, Workload::Set | Workload::Get | Workload::Mixed),
            "keyed driver packets only support SET/GET/Mixed"
        );
        Self {
            connections: keys
                .iter()
                .map(|key| {
                    let set = keyed_set_command(key);
                    let get = keyed_get_command(key);
                    let measured = match workload {
                        Workload::Set => {
                            let case = repeated_case(&set, SET_REPLY, pipeline);
                            WorkloadPackets {
                                odd: ExchangeCase {
                                    request: case.request.clone(),
                                    response: case.response.clone(),
                                },
                                even: case,
                            }
                        }
                        Workload::Get => {
                            let case = repeated_case(&get, GET_REPLY, pipeline);
                            WorkloadPackets {
                                odd: ExchangeCase {
                                    request: case.request.clone(),
                                    response: case.response.clone(),
                                },
                                even: case,
                            }
                        }
                        Workload::Mixed => WorkloadPackets {
                            even: keyed_mixed_case(&set, &get, pipeline, false),
                            odd: keyed_mixed_case(&set, &get, pipeline, true),
                        },
                        _ => unreachable!("keyed workload was prevalidated"),
                    };
                    ConnectionPackets {
                        measured,
                        setup: (!matches!(workload, Workload::Set)).then(|| ExchangeCase {
                            request: set,
                            response: SET_REPLY.to_vec(),
                        }),
                    }
                })
                .collect(),
        }
    }

    fn keyed_mixed_families(pipeline: usize, keys: &[Vec<u8>]) -> (Self, Vec<ExchangeCase>) {
        assert_eq!(
            pipeline, 16,
            "the mixed-family whole-job contract is registered at P16"
        );
        let mut server_setup = Vec::with_capacity(keys.len());
        let connections = keys
            .iter()
            .map(|key| {
                let (measured, setup) = keyed_mixed_families_cases(key);
                server_setup.push(setup);
                ConnectionPackets {
                    measured,
                    setup: None,
                }
            })
            .collect();
        (Self { connections }, server_setup)
    }

    fn for_connection(&self, connection_index: usize) -> &ConnectionPackets {
        if self.connections.len() == 1 {
            &self.connections[0]
        } else {
            self.connections
                .get(connection_index)
                .expect("driver packet count covers every connection")
        }
    }
}

fn push_resp_bulk(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", bytes.len()).as_bytes());
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

fn keyed_set_command(key: &[u8]) -> Vec<u8> {
    let mut command = b"*3\r\n$3\r\nSET\r\n".to_vec();
    push_resp_bulk(&mut command, key);
    push_resp_bulk(&mut command, b"v");
    command
}

fn keyed_get_command(key: &[u8]) -> Vec<u8> {
    let mut command = b"*2\r\n$3\r\nGET\r\n".to_vec();
    push_resp_bulk(&mut command, key);
    command
}

fn keyed_mixed_case(set: &[u8], get: &[u8], pipeline: usize, start_with_get: bool) -> ExchangeCase {
    let mut request = Vec::with_capacity(pipeline * set.len().max(get.len()));
    let mut response = Vec::with_capacity(pipeline * SET_REPLY.len().max(GET_REPLY.len()));
    for index in 0..pipeline {
        let is_get = (index % 2 == 0) == start_with_get;
        if is_get {
            request.extend_from_slice(get);
            response.extend_from_slice(GET_REPLY);
        } else {
            request.extend_from_slice(set);
            response.extend_from_slice(SET_REPLY);
        }
    }
    ExchangeCase { request, response }
}

fn resp_command(parts: &[&[u8]]) -> Vec<u8> {
    let mut command = format!("*{}\r\n", parts.len()).into_bytes();
    for part in parts {
        push_resp_bulk(&mut command, part);
    }
    command
}

fn family_key(base: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(base.len() + suffix.len() + 1);
    key.extend_from_slice(base);
    key.push(b':');
    key.extend_from_slice(suffix);
    key
}

fn exchange_case(steps: &[(&[u8], &[u8])]) -> ExchangeCase {
    let request_len = steps.iter().map(|(request, _)| request.len()).sum();
    let response_len = steps.iter().map(|(_, response)| response.len()).sum();
    let mut request = Vec::with_capacity(request_len);
    let mut response = Vec::with_capacity(response_len);
    for (command, reply) in steps {
        request.extend_from_slice(command);
        response.extend_from_slice(reply);
    }
    ExchangeCase { request, response }
}

fn keyed_mixed_families_cases(base: &[u8]) -> (WorkloadPackets, ExchangeCase) {
    const INTEGER_ZERO_REPLY: &[u8] = b":0\r\n";
    const INTEGER_ONE_REPLY: &[u8] = b":1\r\n";
    const BULK_ZERO_REPLY: &[u8] = b"$1\r\n0\r\n";
    const BULK_A_REPLY: &[u8] = b"$1\r\na\r\n";
    const BULK_B_REPLY: &[u8] = b"$1\r\nb\r\n";

    let string_key = family_key(base, b"string");
    let list_key = family_key(base, b"list");
    let hash_key = family_key(base, b"hash");
    let base_slot = fr_store::crc16_slot(base);
    assert!(
        [&string_key, &list_key, &hash_key]
            .into_iter()
            .all(|key| fr_store::crc16_slot(key) == base_slot),
        "mixed-family keys must preserve their base key's shard"
    );

    let set_zero = resp_command(&[b"SET", &string_key, b"0"]);
    let get_string = resp_command(&[b"GET", &string_key]);
    let incr_string = resp_command(&[b"INCR", &string_key]);
    let lpush_a = resp_command(&[b"LPUSH", &list_key, b"a"]);
    let lpush_b = resp_command(&[b"LPUSH", &list_key, b"b"]);
    let lpop = resp_command(&[b"LPOP", &list_key]);
    let hset_a = resp_command(&[b"HSET", &hash_key, b"f", b"a"]);
    let hset_b = resp_command(&[b"HSET", &hash_key, b"f", b"b"]);
    let hget = resp_command(&[b"HGET", &hash_key, b"f"]);

    // Both orderings are closed state machines: they end with string=1,
    // list absent, and hash[f]=a. That makes every reply byte-exact across
    // warmup, samples, arm rotations, and repeated invocations.
    let even = exchange_case(&[
        (&set_zero, SET_REPLY),
        (&get_string, BULK_ZERO_REPLY),
        (&incr_string, INTEGER_ONE_REPLY),
        (&lpush_a, INTEGER_ONE_REPLY),
        (&hset_a, INTEGER_ZERO_REPLY),
        (&hget, BULK_A_REPLY),
        (&lpop, BULK_A_REPLY),
        (&set_zero, SET_REPLY),
        (&hset_b, INTEGER_ZERO_REPLY),
        (&lpush_b, INTEGER_ONE_REPLY),
        (&get_string, BULK_ZERO_REPLY),
        (&hget, BULK_B_REPLY),
        (&incr_string, INTEGER_ONE_REPLY),
        (&lpop, BULK_B_REPLY),
        (&hset_a, INTEGER_ZERO_REPLY),
        (&hget, BULK_A_REPLY),
    ]);
    let odd = exchange_case(&[
        (&set_zero, SET_REPLY),
        (&hset_b, INTEGER_ZERO_REPLY),
        (&lpush_b, INTEGER_ONE_REPLY),
        (&get_string, BULK_ZERO_REPLY),
        (&hget, BULK_B_REPLY),
        (&incr_string, INTEGER_ONE_REPLY),
        (&lpop, BULK_B_REPLY),
        (&hset_a, INTEGER_ZERO_REPLY),
        (&set_zero, SET_REPLY),
        (&get_string, BULK_ZERO_REPLY),
        (&incr_string, INTEGER_ONE_REPLY),
        (&lpush_a, INTEGER_ONE_REPLY),
        (&hset_a, INTEGER_ZERO_REPLY),
        (&hget, BULK_A_REPLY),
        (&lpop, BULK_A_REPLY),
        (&hget, BULK_A_REPLY),
    ]);
    let setup = ExchangeCase {
        request: hset_a,
        response: INTEGER_ONE_REPLY.to_vec(),
    };
    (WorkloadPackets { even, odd }, setup)
}

fn repeated_case(request: &[u8], response: &[u8], pipeline: usize) -> ExchangeCase {
    ExchangeCase {
        request: request.repeat(pipeline),
        response: response.repeat(pipeline),
    }
}

fn mixed_case(pipeline: usize, start_with_get: bool) -> ExchangeCase {
    let mut request = Vec::with_capacity(pipeline * SET.len().max(GET.len()));
    let mut response = Vec::with_capacity(pipeline * SET_REPLY.len().max(GET_REPLY.len()));
    for index in 0..pipeline {
        let get = (index % 2 == 0) == start_with_get;
        if get {
            request.extend_from_slice(GET);
            response.extend_from_slice(GET_REPLY);
        } else {
            request.extend_from_slice(SET);
            response.extend_from_slice(SET_REPLY);
        }
    }
    ExchangeCase { request, response }
}

enum ClientCommand {
    Prepare {
        packets: Arc<DriverPackets>,
    },
    Run {
        packets: Arc<DriverPackets>,
        groups: usize,
        odd_first: bool,
    },
    Shutdown,
}

struct ClientWorker {
    command: Sender<ClientCommand>,
    complete: Receiver<()>,
    handle: Option<thread::JoinHandle<()>>,
}

struct ClientDriver {
    workers: Vec<ClientWorker>,
}

#[derive(Clone, Copy)]
struct ClientShape {
    connections: usize,
    driver_threads: usize,
}

impl ClientDriver {
    fn new(port: u16, shape: ClientShape) -> Self {
        assert!(
            (1..=shape.connections).contains(&shape.driver_threads),
            "client thread count must be in 1..={}",
            shape.connections
        );
        let mut workers = Vec::with_capacity(shape.driver_threads);
        let mut next_connection_index = 0usize;
        for worker_index in 0..shape.driver_threads {
            let client_count = shape.connections / shape.driver_threads
                + usize::from(worker_index < shape.connections % shape.driver_threads);
            let clients = (0..client_count)
                .map(|_| {
                    let connection_index = next_connection_index;
                    next_connection_index += 1;
                    (connection_index, connect(port))
                })
                .collect();
            let (command_tx, command_rx) = mpsc::channel();
            let (complete_tx, complete_rx) = mpsc::channel();
            let handle = thread::Builder::new()
                .name(format!("bench-client-{worker_index}"))
                .spawn(move || {
                    client_worker(clients, command_rx, complete_tx);
                })
                .expect("spawn benchmark client worker");
            workers.push(ClientWorker {
                command: command_tx,
                complete: complete_rx,
                handle: Some(handle),
            });
        }
        Self { workers }
    }

    fn prepare(&self, packets: &Arc<DriverPackets>) {
        for worker in &self.workers {
            worker
                .command
                .send(ClientCommand::Prepare {
                    packets: Arc::clone(packets),
                })
                .expect("dispatch benchmark client setup");
        }
        for worker in &self.workers {
            worker
                .complete
                .recv()
                .expect("benchmark client setup completed");
        }
    }

    fn run(&self, packets: &Arc<DriverPackets>, groups: usize, odd_first: bool) -> Duration {
        let start = Instant::now();
        for worker in &self.workers {
            worker
                .command
                .send(ClientCommand::Run {
                    packets: Arc::clone(packets),
                    groups,
                    odd_first,
                })
                .expect("dispatch benchmark client work");
        }
        for worker in &self.workers {
            worker
                .complete
                .recv()
                .expect("benchmark client worker completed");
        }
        start.elapsed()
    }
}

impl Drop for ClientDriver {
    fn drop(&mut self) {
        for worker in &self.workers {
            let _ = worker.command.send(ClientCommand::Shutdown);
        }
        for worker in &mut self.workers {
            if let Some(handle) = worker.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

fn client_worker(
    mut clients: Vec<(usize, TcpStream)>,
    commands: Receiver<ClientCommand>,
    complete: Sender<()>,
) {
    while let Ok(command) = commands.recv() {
        if let ClientCommand::Prepare { packets } = command {
            let mut response = Vec::new();
            for (connection_index, client) in &mut clients {
                let Some(setup) = &packets.for_connection(*connection_index).setup else {
                    continue;
                };
                client
                    .write_all(&setup.request)
                    .expect("write keyed benchmark setup");
                response.resize(setup.response.len(), 0);
                client
                    .read_exact(&mut response)
                    .expect("read keyed benchmark setup response");
                assert_eq!(
                    response, setup.response,
                    "keyed setup diverged from the RESP oracle"
                );
            }
            complete
                .send(())
                .expect("report benchmark setup completion");
            continue;
        }
        let ClientCommand::Run {
            packets,
            groups,
            odd_first,
        } = command
        else {
            return;
        };
        let mut response = Vec::new();
        for group in 0..groups {
            let odd = (group % 2 == 1) ^ odd_first;
            for (connection_index, client) in &mut clients {
                let measured = &packets.for_connection(*connection_index).measured;
                let case = if odd { &measured.odd } else { &measured.even };
                client
                    .write_all(black_box(case.request.as_slice()))
                    .expect("write request group");
            }
            for (connection_index, client) in &mut clients {
                let measured = &packets.for_connection(*connection_index).measured;
                let case = if odd { &measured.odd } else { &measured.even };
                response.resize(case.response.len(), 0);
                client
                    .read_exact(&mut response)
                    .expect("read complete response group");
                assert_eq!(
                    response, case.response,
                    "server returned bytes that diverge from the RESP oracle"
                );
                black_box(response.as_slice());
            }
        }
        complete
            .send(())
            .expect("report benchmark client completion");
    }
}

struct Server {
    arm: Arm,
    child: Child,
    port: u16,
    clients: Option<ClientDriver>,
    stderr_path: PathBuf,
}

impl Server {
    fn spawn(
        fr_binary: &Path,
        redis_binary: &Path,
        arm: Arm,
        root: &Path,
        server_core: usize,
        client_shape: ClientShape,
        command_floor_ab: CommandFloorAb,
    ) -> Self {
        Self::spawn_with_options(
            fr_binary,
            redis_binary,
            arm,
            root,
            &[server_core],
            client_shape,
            command_floor_ab,
            None,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_with_options(
        fr_binary: &Path,
        redis_binary: &Path,
        arm: Arm,
        root: &Path,
        server_cpus: &[usize],
        client_shape: ClientShape,
        command_floor_ab: CommandFloorAb,
        sharded_set_get_workers: Option<usize>,
        force_io_uring_output: bool,
    ) -> Self {
        assert!(!server_cpus.is_empty(), "server affinity cannot be empty");
        let runtime_dir = root.join(arm.name());
        fs::create_dir_all(&runtime_dir).expect("create unique server runtime directory");
        let stderr_path = runtime_dir.join("stderr.log");
        let stderr = File::create(&stderr_path).expect("create unique server stderr log");
        let port = free_port();
        let server_cpu_list = server_cpus
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut command = Command::new("taskset");
        command
            .args(["-c", &server_cpu_list])
            .arg(if matches!(arm, Arm::Redis) {
                redis_binary
            } else {
                fr_binary
            })
            .args(["--bind", "127.0.0.1", "--port", &port.to_string()]);
        if matches!(arm, Arm::Redis) {
            command.args(["--save", "", "--appendonly", "no"]);
        }
        if !matches!(arm, Arm::Redis)
            && (force_io_uring_output
                || (sharded_set_get_workers.is_none()
                    && (matches!(arm, Arm::IoUring)
                        || !matches!(command_floor_ab, CommandFloorAb::None))))
        {
            command.arg(
                std::env::var("FR_URING_AB_FLAG").unwrap_or_else(|_| IO_URING_FLAG.to_owned()),
            );
        }
        if let Some(workers) = sharded_set_get_workers {
            assert!(
                !matches!(arm, Arm::Redis),
                "Redis cannot receive FrankenRedis shard flags"
            );
            command
                .arg("--experimental-sharded-set-get-workers")
                .arg(workers.to_string());
        }
        if matches!(command_floor_ab, CommandFloorAb::BitposRange)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(BITPOS_RANGE_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::BitfieldRoTwoGet)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::ObjectEncoding)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(OBJECT_ENCODING_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::ObjectRefcount)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(OBJECT_REFCOUNT_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::Dbsize)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(DBSIZE_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::Echo) && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(ECHO_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::WaitZero)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(WAIT_ZERO_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::XtrimMinidNoop)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(XTRIM_MINID_NOOP_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::XtrimMinidNoopFloor)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::XdelMissingFloor)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(XDEL_MISSING_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::XackMissingFloor)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(XACK_MISSING_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::XrangeZeroFloor)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(XRANGE_ZERO_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::XrevrangeZeroFloor)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(XREVRANGE_ZERO_FLOOR_CONTROL_ENV, "1");
        }
        if matches!(command_floor_ab, CommandFloorAb::LrangeFloor)
            && matches!(arm, Arm::MioA | Arm::MioB)
        {
            command.env(LRANGE_FLOOR_CONTROL_ENV, "1");
        }
        command
            .current_dir(&runtime_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));

        let child = command.spawn().expect("spawn benchmark server arm");
        let mut server = Self {
            arm,
            child,
            port,
            clients: None,
            stderr_path,
        };
        server.wait_until_ready();
        server.clients = Some(ClientDriver::new(port, client_shape));
        server
    }

    fn replace_clients(&mut self, client_shape: ClientShape) {
        self.clients = None;
        self.clients = Some(ClientDriver::new(self.port, client_shape));
    }

    fn client_thread_count(&self) -> usize {
        self.clients
            .as_ref()
            .expect("benchmark clients initialized")
            .workers
            .len()
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn cpu_ns(&self) -> u64 {
        let task_root = format!("/proc/{}/task", self.pid());
        let mut total = 0_u64;
        let mut observed = 0usize;
        for entry in fs::read_dir(&task_root).expect("read server task directory") {
            let entry = entry.expect("read server task entry");
            let schedstat = match fs::read_to_string(entry.path().join("schedstat")) {
                Ok(schedstat) => schedstat,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => panic!("read server task schedstat: {error}"),
            };
            let task_cpu_ns = schedstat
                .split_whitespace()
                .next()
                .expect("task schedstat contains execution time")
                .parse::<u64>()
                .expect("parse task CPU nanoseconds");
            total = total
                .checked_add(task_cpu_ns)
                .expect("aggregate server CPU time overflow");
            observed += 1;
        }
        assert!(observed > 0, "server process exposed no measurable tasks");
        total
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll server startup") {
                let stderr = fs::read_to_string(&self.stderr_path).unwrap_or_default();
                panic!(
                    "{} server exited during startup with {status}: {stderr}",
                    self.arm.name()
                );
            }
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let stderr = fs::read_to_string(&self.stderr_path).unwrap_or_default();
        panic!(
            "{} server on port {} did not become ready: {stderr}",
            self.arm.name(),
            self.port
        );
    }

    fn assert_flag_reached_process(&self) {
        assert!(
            !matches!(self.arm, Arm::Redis),
            "Redis does not accept the FrankenRedis io_uring flag"
        );
        let cmdline = fs::read(format!("/proc/{}/cmdline", self.pid()))
            .expect("read candidate process command line");
        let flag = std::env::var("FR_URING_AB_FLAG").unwrap_or_else(|_| IO_URING_FLAG.to_owned());
        assert!(
            cmdline
                .split(|byte| *byte == 0)
                .any(|arg| arg == flag.as_bytes()),
            "{} process did not receive {flag}",
            self.arm.name()
        );
        println!(
            "FRANKENREDIS_FLAG arm={} pid={} flag={flag}",
            self.arm.name(),
            self.pid()
        );
    }

    fn assert_sharded_set_get_workers_reached_process(&self, workers: usize) {
        assert!(
            !matches!(self.arm, Arm::Redis),
            "Redis does not accept the FrankenRedis shard flag"
        );
        let cmdline = fs::read(format!("/proc/{}/cmdline", self.pid()))
            .expect("read candidate process command line");
        let args = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .collect::<Vec<_>>();
        let flag_index = args
            .iter()
            .position(|arg| *arg == b"--experimental-sharded-set-get-workers")
            .expect("candidate process received the shard flag");
        let expected_workers = workers.to_string();
        assert_eq!(
            args.get(flag_index + 1).copied(),
            Some(expected_workers.as_bytes()),
            "candidate process received the requested shard count"
        );
        println!(
            "FRANKENREDIS_SHARD_FLAG arm={} pid={} workers={workers}",
            self.arm.name(),
            self.pid()
        );
    }

    fn assert_environment_value(&self, name: &str, expected: Option<&str>) {
        assert!(
            !matches!(self.arm, Arm::Redis),
            "Redis environment is outside the same-ELF control contract"
        );
        let environ = fs::read(format!("/proc/{}/environ", self.pid()))
            .expect("read FrankenRedis process environment");
        let prefix = format!("{name}=");
        let actual = environ.split(|byte| *byte == 0).find_map(|entry| {
            entry
                .strip_prefix(prefix.as_bytes())
                .map(|value| String::from_utf8_lossy(value).into_owned())
        });
        assert_eq!(
            actual.as_deref(),
            expected,
            "{} process environment diverged for {name}",
            self.arm.name()
        );
        println!(
            "FRANKENREDIS_ENV arm={} pid={} name={name} value={:?}",
            self.arm.name(),
            self.pid(),
            actual
        );
    }

    fn executing_elf_sha256(&self) -> String {
        hash_path(&PathBuf::from(format!("/proc/{}/exe", self.child.id())))
    }

    fn observed_thread_count(&self) -> usize {
        fs::read_dir(format!("/proc/{}/task", self.pid()))
            .expect("read server task directory")
            .count()
    }

    fn sharded_worker_cpu_ns(&self) -> HashMap<u32, u64> {
        let mut cpu_ns = HashMap::new();
        let task_root = format!("/proc/{}/task", self.pid());
        for entry in fs::read_dir(&task_root).expect("read server task directory") {
            let entry = entry.expect("read server task entry");
            let tid = entry
                .file_name()
                .to_string_lossy()
                .parse::<u32>()
                .expect("task directory is a TID");
            let comm =
                fs::read_to_string(entry.path().join("comm")).expect("read server task comm");
            if !comm.trim().starts_with("fr-set-get-sha") {
                continue;
            }
            let schedstat = fs::read_to_string(entry.path().join("schedstat"))
                .expect("read sharded worker schedstat");
            let task_cpu_ns = schedstat
                .split_whitespace()
                .next()
                .expect("sharded worker schedstat contains execution time")
                .parse::<u64>()
                .expect("parse sharded worker CPU nanoseconds");
            cpu_ns.insert(tid, task_cpu_ns);
        }
        cpu_ns
    }

    fn affinity_cpus(&self) -> Vec<usize> {
        allowed_cpus_for_pid(self.pid())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.clients.take();
        if matches!(self.child.try_wait(), Ok(None)) {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", self.port)) {
                let _ = stream.write_all(SHUTDOWN);
            }
            for _ in 0..100 {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => return,
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                }
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("read ephemeral port")
        .port()
}

fn connect(port: u16) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect benchmark client");
    stream
        .set_nodelay(true)
        .expect("set benchmark client TCP_NODELAY");
    stream
        .set_read_timeout(Some(BENCH_CLIENT_IO_TIMEOUT))
        .expect("set benchmark client read timeout");
    stream
        .set_write_timeout(Some(BENCH_CLIENT_IO_TIMEOUT))
        .expect("set benchmark client write timeout");
    stream
}

fn time_block(
    server: &mut Server,
    packets: &Arc<DriverPackets>,
    groups: usize,
    odd_first: bool,
) -> Duration {
    server
        .clients
        .as_ref()
        .expect("benchmark clients initialized")
        .run(packets, groups, odd_first)
}

fn exchange_one(server: &mut Server, request: &[u8], expected: &[u8]) {
    let mut stream = connect(server.port);
    stream.write_all(request).expect("write setup request");
    let mut response = vec![0_u8; expected.len()];
    if let Err(error) = stream.read_exact(&mut response) {
        let prefix_len = request.len().min(128);
        panic!(
            "read setup response: arm={} request_len={} request_prefix={:?} \
expected_len={} partial_response={:?}: {error}",
            server.arm.name(),
            request.len(),
            &request[..prefix_len],
            expected.len(),
            response
        );
    }
    assert_eq!(
        response,
        expected,
        "setup reply diverged for arm={}",
        server.arm.name()
    );
}

fn apply_server_setup(servers: &mut [Server; 4], setup: &[ExchangeCase]) {
    for server in servers {
        for case in setup {
            exchange_one(server, &case.request, &case.response);
        }
    }
}

fn seeded_stream_prefill() -> ExchangeCase {
    let mut request = Vec::new();
    let mut response = Vec::new();

    // Make each prefill idempotent without accepting an arm-dependent DEL
    // reply: SET guarantees that DEL removes exactly one key.
    request.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$2\r\nxs\r\n$4\r\nseed\r\n");
    response.extend_from_slice(SET_REPLY);
    request.extend_from_slice(b"*2\r\n$3\r\nDEL\r\n$2\r\nxs\r\n");
    response.extend_from_slice(b":1\r\n");

    for id in 1..=XTRIM_MINID_NOOP_PREFILL_ENTRIES {
        let stream_id = format!("{id}-0");
        let command = format!(
            "*5\r\n$4\r\nXADD\r\n$2\r\nxs\r\n${}\r\n{stream_id}\r\n\
$1\r\nf\r\n$1\r\nv\r\n",
            stream_id.len()
        );
        request.extend_from_slice(command.as_bytes());
        let reply = format!("${}\r\n{stream_id}\r\n", stream_id.len());
        response.extend_from_slice(reply.as_bytes());
    }
    ExchangeCase { request, response }
}

fn seeded_pending_prefill() -> ExchangeCase {
    let request = XREADGROUP_ALL_XS_G_C.to_vec();
    let mut response = Vec::new();
    response.extend_from_slice(b"*1\r\n*2\r\n$2\r\nxs\r\n*1000\r\n");
    for id in 1..=XTRIM_MINID_NOOP_PREFILL_ENTRIES {
        let stream_id = format!("{id}-0");
        let record = format!(
            "*2\r\n${}\r\n{stream_id}\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n",
            stream_id.len()
        );
        response.extend_from_slice(record.as_bytes());
    }
    ExchangeCase { request, response }
}

fn hash_prefill(value: &str) -> ExchangeCase {
    assert_eq!(value.len(), 1, "hash fixture values must stay one byte");
    let mut request = b"*3\r\n$3\r\nSET\r\n$1\r\nh\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n$1\r\nh\r\n"
        .to_vec();
    let header = format!(
        "*{}\r\n$4\r\nHSET\r\n$1\r\nh\r\n",
        MISSING_FIELD_HASH_FIELDS * 2 + 2
    );
    request.extend_from_slice(header.as_bytes());
    for index in 0..MISSING_FIELD_HASH_FIELDS {
        let field = format!("f{index:03}");
        let pair = format!("$4\r\n{field}\r\n$1\r\n{value}\r\n");
        request.extend_from_slice(pair.as_bytes());
    }
    ExchangeCase {
        request,
        response: format!("+OK\r\n:1\r\n:{MISSING_FIELD_HASH_FIELDS}\r\n").into_bytes(),
    }
}

fn hkeys_fields_reply() -> Vec<u8> {
    let mut response = format!("*{MISSING_FIELD_HASH_FIELDS}\r\n").into_bytes();
    for index in 0..MISSING_FIELD_HASH_FIELDS {
        let field = format!("f{index:03}");
        let value = format!("$4\r\n{field}\r\n");
        response.extend_from_slice(value.as_bytes());
    }
    response
}

fn hvals_fields_reply() -> Vec<u8> {
    let mut response = format!("*{MISSING_FIELD_HASH_FIELDS}\r\n").into_bytes();
    for _ in 0..MISSING_FIELD_HASH_FIELDS {
        response.extend_from_slice(b"$1\r\n1\r\n");
    }
    response
}

fn hgetall_fields_reply() -> Vec<u8> {
    let mut response = format!("*{}\r\n", MISSING_FIELD_HASH_FIELDS * 2).into_bytes();
    for index in 0..MISSING_FIELD_HASH_FIELDS {
        let field = format!("f{index:03}");
        let pair = format!("$4\r\n{field}\r\n$1\r\n1\r\n");
        response.extend_from_slice(pair.as_bytes());
    }
    response
}

fn hscan_all_fields_reply() -> Vec<u8> {
    let mut response =
        format!("*2\r\n$1\r\n0\r\n*{}\r\n", MISSING_FIELD_HASH_FIELDS * 2).into_bytes();
    for index in 0..MISSING_FIELD_HASH_FIELDS {
        let field = format!("f{index:03}");
        let pair = format!("$4\r\n{field}\r\n$1\r\n1\r\n");
        response.extend_from_slice(pair.as_bytes());
    }
    response
}

fn scan0_prefill(workload: Workload) -> ExchangeCase {
    let key: &[u8] = match workload {
        Workload::SscanCursorZero => b"s0",
        Workload::HscanCursorZero => b"h0",
        Workload::ZscanCursorZero => b"z0",
        _ => unreachable!("compact scan fixture requires a cursor-zero scan workload"),
    };
    let mut request = resp_command(&[b"SET", key, b"seed"]);
    request.extend_from_slice(&resp_command(&[b"DEL", key]));

    match workload {
        Workload::SscanCursorZero => {
            request.extend_from_slice(format!("*{}\r\n", SCAN0_MEMBERS + 2).as_bytes());
            push_resp_bulk(&mut request, b"SADD");
            push_resp_bulk(&mut request, key);
            for index in 0..SCAN0_MEMBERS {
                push_resp_bulk(&mut request, index.to_string().as_bytes());
            }
        }
        Workload::HscanCursorZero => {
            request.extend_from_slice(format!("*{}\r\n", SCAN0_MEMBERS * 2 + 2).as_bytes());
            push_resp_bulk(&mut request, b"HSET");
            push_resp_bulk(&mut request, key);
            for index in 0..SCAN0_MEMBERS {
                push_resp_bulk(&mut request, format!("f{index:02}").as_bytes());
                push_resp_bulk(&mut request, format!("v{index:02}").as_bytes());
            }
        }
        Workload::ZscanCursorZero => {
            request.extend_from_slice(format!("*{}\r\n", SCAN0_MEMBERS * 2 + 2).as_bytes());
            push_resp_bulk(&mut request, b"ZADD");
            push_resp_bulk(&mut request, key);
            for index in 0..SCAN0_MEMBERS {
                push_resp_bulk(&mut request, index.to_string().as_bytes());
                push_resp_bulk(&mut request, format!("m{index:02}").as_bytes());
            }
        }
        _ => unreachable!("compact scan fixture requires a cursor-zero scan workload"),
    }

    ExchangeCase {
        request,
        response: format!("+OK\r\n:1\r\n:{SCAN0_MEMBERS}\r\n").into_bytes(),
    }
}

fn sscan_cursor_zero_reply() -> Vec<u8> {
    let mut response = format!("*2\r\n$1\r\n0\r\n*{SCAN0_MEMBERS}\r\n").into_bytes();
    for index in 0..SCAN0_MEMBERS {
        push_resp_bulk(&mut response, index.to_string().as_bytes());
    }
    response
}

fn hscan_cursor_zero_reply() -> Vec<u8> {
    let mut response = format!("*2\r\n$1\r\n0\r\n*{}\r\n", SCAN0_MEMBERS * 2).into_bytes();
    for index in 0..SCAN0_MEMBERS {
        push_resp_bulk(&mut response, format!("f{index:02}").as_bytes());
        push_resp_bulk(&mut response, format!("v{index:02}").as_bytes());
    }
    response
}

fn zscan_cursor_zero_reply() -> Vec<u8> {
    let mut response = format!("*2\r\n$1\r\n0\r\n*{}\r\n", SCAN0_MEMBERS * 2).into_bytes();
    for index in 0..SCAN0_MEMBERS {
        push_resp_bulk(&mut response, format!("m{index:02}").as_bytes());
        push_resp_bulk(&mut response, index.to_string().as_bytes());
    }
    response
}

fn scan0_request_reply(workload: Workload) -> (&'static [u8], Vec<u8>) {
    match workload {
        Workload::SscanCursorZero => (SSCAN_CURSOR_ZERO, sscan_cursor_zero_reply()),
        Workload::HscanCursorZero => (HSCAN_CURSOR_ZERO, hscan_cursor_zero_reply()),
        Workload::ZscanCursorZero => (ZSCAN_CURSOR_ZERO, zscan_cursor_zero_reply()),
        _ => unreachable!("compact scan fixture requires a cursor-zero scan workload"),
    }
}

fn assert_scan0_fixture(server: &mut Server, workload: Workload) {
    let (card, encoding, encoding_reply, boundaries, boundaries_reply, pttl) = match workload {
        Workload::SscanCursorZero => (
            SSCAN0_CARD,
            SSCAN0_ENCODING,
            SSCAN0_ENCODING_REPLY,
            SSCAN0_BOUNDARIES,
            SSCAN0_BOUNDARIES_REPLY,
            SSCAN0_PTTL,
        ),
        Workload::HscanCursorZero => (
            HSCAN0_CARD,
            HSCAN0_ENCODING,
            SCAN0_LISTPACK_ENCODING_REPLY,
            HSCAN0_BOUNDARIES,
            HSCAN0_BOUNDARIES_REPLY,
            HSCAN0_PTTL,
        ),
        Workload::ZscanCursorZero => (
            ZSCAN0_CARD,
            ZSCAN0_ENCODING,
            SCAN0_LISTPACK_ENCODING_REPLY,
            ZSCAN0_BOUNDARIES,
            ZSCAN0_BOUNDARIES_REPLY,
            ZSCAN0_PTTL,
        ),
        _ => unreachable!("compact scan fixture requires a cursor-zero scan workload"),
    };
    exchange_one(server, card, SCAN0_CARD_REPLY);
    exchange_one(server, encoding, encoding_reply);
    exchange_one(server, boundaries, boundaries_reply);
    exchange_one(server, pttl, PTTL_PERSISTENT_REPLY);
    let (request, response) = scan0_request_reply(workload);
    exchange_one(server, request, &response);
}

fn zrangebylex_range_reply() -> Vec<u8> {
    let count = ZRANGEBYLEX_RANGE_END - ZRANGEBYLEX_RANGE_START + 1;
    let mut response = format!("*{count}\r\n").into_bytes();
    for member in ZRANGEBYLEX_RANGE_START..=ZRANGEBYLEX_RANGE_END {
        let member = format!("{member:05}");
        let value = format!("$5\r\n{member}\r\n");
        response.extend_from_slice(value.as_bytes());
    }
    response
}

fn zrevrangebylex_range_reply() -> Vec<u8> {
    let count = ZRANGEBYLEX_RANGE_END - ZRANGEBYLEX_RANGE_START + 1;
    let mut response = format!("*{count}\r\n").into_bytes();
    for member in (ZRANGEBYLEX_RANGE_START..=ZRANGEBYLEX_RANGE_END).rev() {
        let member = format!("{member:05}");
        let value = format!("$5\r\n{member}\r\n");
        response.extend_from_slice(value.as_bytes());
    }
    response
}

fn zrangebyscore_range_reply() -> Vec<u8> {
    let count = ZRANGEBYSCORE_RANGE_END - ZRANGEBYSCORE_RANGE_START + 1;
    let mut response = format!("*{count}\r\n").into_bytes();
    for member in ZRANGEBYSCORE_RANGE_START..=ZRANGEBYSCORE_RANGE_END {
        let member = format!("{member:05}");
        let value = format!("$5\r\n{member}\r\n");
        response.extend_from_slice(value.as_bytes());
    }
    response
}

fn zrevrangebyscore_range_reply() -> Vec<u8> {
    let count = ZRANGEBYSCORE_RANGE_END - ZRANGEBYSCORE_RANGE_START + 1;
    let mut response = format!("*{count}\r\n").into_bytes();
    for member in (ZRANGEBYSCORE_RANGE_START..=ZRANGEBYSCORE_RANGE_END).rev() {
        let member = format!("{member:05}");
        let value = format!("$5\r\n{member}\r\n");
        response.extend_from_slice(value.as_bytes());
    }
    response
}

fn zrevrange_rank_range_reply() -> Vec<u8> {
    let count = ZREVRANGE_RANK_END - ZREVRANGE_RANK_START + 1;
    let first_member = ZRANGEBYSCORE_MEMBERS - 1 - ZREVRANGE_RANK_START;
    let last_member = ZRANGEBYSCORE_MEMBERS - 1 - ZREVRANGE_RANK_END;
    let mut response = format!("*{count}\r\n").into_bytes();
    for member in (last_member..=first_member).rev() {
        let member = format!("{member:05}");
        let value = format!("$5\r\n{member}\r\n");
        response.extend_from_slice(value.as_bytes());
    }
    response
}

fn zset_algebra_member_reply(start: usize, end: usize) -> Vec<u8> {
    let mut response = format!("*{}\r\n", end - start).into_bytes();
    for member in start..end {
        let member = member.to_string();
        push_resp_bulk(&mut response, member.as_bytes());
    }
    response
}

fn zdiff_two_key_reply() -> Vec<u8> {
    zset_algebra_member_reply(0, ZINTERCARD_SOURCE_B_START)
}

fn zinter_two_key_reply() -> Vec<u8> {
    zset_algebra_member_reply(ZINTERCARD_SOURCE_B_START, ZINTERCARD_SOURCE_MEMBERS)
}

fn lindex_middle_prefill() -> ExchangeCase {
    let mut request = b"*3\r\n$3\r\nSET\r\n$1\r\nl\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n$1\r\nl\r\n"
        .to_vec();
    let header = format!(
        "*{}\r\n$5\r\nRPUSH\r\n$1\r\nl\r\n",
        LINDEX_MIDDLE_ELEMENTS + 2
    );
    request.extend_from_slice(header.as_bytes());
    for index in 0..LINDEX_MIDDLE_ELEMENTS {
        let element = format!("v{index:03}");
        let value = format!("$4\r\n{element}\r\n");
        request.extend_from_slice(value.as_bytes());
    }
    ExchangeCase {
        request,
        response: format!("+OK\r\n:1\r\n:{LINDEX_MIDDLE_ELEMENTS}\r\n").into_bytes(),
    }
}

fn prefill_two_dense_hll_sources(server: &mut Server) {
    for key in ["h1", "h2", "dst"] {
        let reset = format!(
            "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n${}\r\n{key}\r\n",
            key.len(),
            key.len()
        );
        exchange_one(server, reset.as_bytes(), b"+OK\r\n:1\r\n");
    }

    for prefix in ['a', 'b'] {
        let key = if prefix == 'a' { "h1" } else { "h2" };
        for batch_start in (0..PFMERGE_DENSE_SOURCE_ELEMENTS).step_by(PFMERGE_DENSE_PREFILL_BATCH) {
            let batch_end =
                (batch_start + PFMERGE_DENSE_PREFILL_BATCH).min(PFMERGE_DENSE_SOURCE_ELEMENTS);
            let header = format!(
                "*{}\r\n$5\r\nPFADD\r\n$2\r\n{key}\r\n",
                batch_end - batch_start + 2
            );
            let mut request = header.into_bytes();
            for index in batch_start..batch_end {
                let element = format!("{prefix}{index:04}");
                let value = format!("$5\r\n{element}\r\n");
                request.extend_from_slice(value.as_bytes());
            }
            exchange_one(server, &request, b":1\r\n");
        }
    }
}

fn bitcount_one_mib_prefill() -> ExchangeCase {
    let mut request =
        format!("*3\r\n$3\r\nSET\r\n$10\r\nbitcount:k\r\n${BITCOUNT_ONE_MIB_BYTES}\r\n")
            .into_bytes();
    request.resize(request.len() + BITCOUNT_ONE_MIB_BYTES, 0xaa);
    request.extend_from_slice(b"\r\n");
    ExchangeCase {
        request,
        response: SET_REPLY.to_vec(),
    }
}

fn bitcount_one_mib_get_reply() -> Vec<u8> {
    let mut response = format!("${BITCOUNT_ONE_MIB_BYTES}\r\n").into_bytes();
    response.resize(response.len() + BITCOUNT_ONE_MIB_BYTES, 0xaa);
    response.extend_from_slice(b"\r\n");
    response
}

fn prefill_zrangebylex_source(server: &mut Server) {
    exchange_one(
        server,
        b"*3\r\n$3\r\nSET\r\n$4\r\nlexz\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n$4\r\nlexz\r\n",
        b"+OK\r\n:1\r\n",
    );
    for batch_start in (0..ZRANGEBYLEX_MEMBERS).step_by(ZRANGEBYLEX_PREFILL_BATCH) {
        let batch_end = (batch_start + ZRANGEBYLEX_PREFILL_BATCH).min(ZRANGEBYLEX_MEMBERS);
        let mut request = format!(
            "*{}\r\n$4\r\nZADD\r\n$4\r\nlexz\r\n",
            (batch_end - batch_start) * 2 + 2
        )
        .into_bytes();
        for member in batch_start..batch_end {
            let member = format!("{member:05}");
            request.extend_from_slice(b"$1\r\n0\r\n$5\r\n");
            request.extend_from_slice(member.as_bytes());
            request.extend_from_slice(b"\r\n");
        }
        let response = format!(":{}\r\n", batch_end - batch_start);
        exchange_one(server, &request, response.as_bytes());
    }
}

fn assert_lexz_skiplist_fixture(server: &mut Server) {
    exchange_one(server, ZRANGEBYLEX_ZCARD, ZRANGEBYLEX_ZCARD_REPLY);
    exchange_one(server, ZRANGEBYLEX_PTTL, PTTL_PERSISTENT_REPLY);
    exchange_one(server, ZRANGEBYLEX_ENCODING, ZSET_SKIPLIST_ENCODING_REPLY);
}

fn prefill_zrangebyscore_source(server: &mut Server) {
    exchange_one(
        server,
        b"*3\r\n$3\r\nSET\r\n$6\r\nscorez\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n$6\r\nscorez\r\n",
        b"+OK\r\n:1\r\n",
    );
    for batch_start in (0..ZRANGEBYSCORE_MEMBERS).step_by(ZRANGEBYSCORE_PREFILL_BATCH) {
        let batch_end = (batch_start + ZRANGEBYSCORE_PREFILL_BATCH).min(ZRANGEBYSCORE_MEMBERS);
        let mut request = format!(
            "*{}\r\n$4\r\nZADD\r\n$6\r\nscorez\r\n",
            (batch_end - batch_start) * 2 + 2
        )
        .into_bytes();
        for index in batch_start..batch_end {
            let score = index.to_string();
            let member = format!("{index:05}");
            push_resp_bulk(&mut request, score.as_bytes());
            push_resp_bulk(&mut request, member.as_bytes());
        }
        let response = format!(":{}\r\n", batch_end - batch_start);
        exchange_one(server, &request, response.as_bytes());
    }
}

fn assert_scorez_skiplist_fixture(server: &mut Server) {
    exchange_one(server, ZRANGEBYSCORE_ZCARD, ZRANGEBYSCORE_ZCARD_REPLY);
    exchange_one(server, ZRANGEBYSCORE_PTTL, PTTL_PERSISTENT_REPLY);
    exchange_one(server, ZRANGEBYSCORE_ENCODING, ZSET_SKIPLIST_ENCODING_REPLY);
}

fn prefill_mixed_setstore_sources(server: &mut Server, large_key: &str, large_start: usize) {
    for key in ["small", large_key, "dst"] {
        let reset = format!(
            "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n${}\r\n{key}\r\n",
            key.len(),
            key.len()
        );
        exchange_one(server, reset.as_bytes(), b"+OK\r\n:1\r\n");
    }

    for (key, start, members) in [
        ("small", 0, SUNIONSTORE_SMALL_MEMBERS),
        (large_key, large_start, SUNIONSTORE_LARGE_MEMBERS),
    ] {
        for batch_start in (0..members).step_by(SUNIONSTORE_PREFILL_BATCH) {
            let batch_end = (batch_start + SUNIONSTORE_PREFILL_BATCH).min(members);
            let header = format!(
                "*{}\r\n$4\r\nSADD\r\n${}\r\n{key}\r\n",
                batch_end - batch_start + 2,
                key.len()
            );
            let mut request = header.into_bytes();
            for offset in batch_start..batch_end {
                let member = (start + offset).to_string();
                let value = format!("${}\r\n{member}\r\n", member.len());
                request.extend_from_slice(value.as_bytes());
            }
            let response = format!(":{}\r\n", batch_end - batch_start);
            exchange_one(server, &request, response.as_bytes());
        }
    }
}

fn prefill_zintercard_sources(server: &mut Server) {
    for key in ["za", "zb"] {
        let reset = format!(
            "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n$4\r\nseed\r\n\
*2\r\n$3\r\nDEL\r\n${}\r\n{key}\r\n",
            key.len(),
            key.len()
        );
        exchange_one(server, reset.as_bytes(), b"+OK\r\n:1\r\n");
    }

    for (key, start) in [("za", 0), ("zb", ZINTERCARD_SOURCE_B_START)] {
        for batch_start in (0..ZINTERCARD_SOURCE_MEMBERS).step_by(ZINTERCARD_PREFILL_BATCH) {
            let batch_end = (batch_start + ZINTERCARD_PREFILL_BATCH).min(ZINTERCARD_SOURCE_MEMBERS);
            let header = format!(
                "*{}\r\n$4\r\nZADD\r\n${}\r\n{key}\r\n",
                (batch_end - batch_start) * 2 + 2,
                key.len()
            );
            let mut request = header.into_bytes();
            for offset in batch_start..batch_end {
                let member = (start + offset).to_string();
                let pair = format!(
                    "${}\r\n{member}\r\n${}\r\n{member}\r\n",
                    member.len(),
                    member.len()
                );
                request.extend_from_slice(pair.as_bytes());
            }
            let response = format!(":{}\r\n", batch_end - batch_start);
            exchange_one(server, &request, response.as_bytes());
        }
    }
}

fn prefill_selective_prefix_keyspace(server: &mut Server, workload: Workload) {
    exchange_one(
        server,
        b"*3\r\n$3\r\nSET\r\n$20\r\ntenant:needle:000000\r\n$1\r\nv\r\n",
        SET_REPLY,
    );

    for batch_start in (0..SELECTIVE_PREFIX_DECOYS).step_by(SELECTIVE_PREFIX_PREFILL_BATCH) {
        let batch_end = (batch_start + SELECTIVE_PREFIX_PREFILL_BATCH).min(SELECTIVE_PREFIX_DECOYS);
        let mut request =
            format!("*{}\r\n$4\r\nMSET\r\n", 1 + 2 * (batch_end - batch_start)).into_bytes();
        for index in batch_start..batch_end {
            let key = format!("tenant:decoy:{index:06}");
            push_resp_bulk(&mut request, key.as_bytes());
            push_resp_bulk(&mut request, b"v");
        }
        exchange_one(server, &request, SET_REPLY);
    }

    let expected_dbsize = format!(":{}\r\n", SELECTIVE_PREFIX_DECOYS + 2);
    exchange_one(server, DBSIZE, expected_dbsize.as_bytes());

    let last_decoy = format!("tenant:decoy:{:06}", SELECTIVE_PREFIX_DECOYS - 1);
    let mut boundary_request = b"*4\r\n$6\r\nEXISTS\r\n".to_vec();
    push_resp_bulk(&mut boundary_request, b"tenant:decoy:000000");
    push_resp_bulk(&mut boundary_request, last_decoy.as_bytes());
    push_resp_bulk(&mut boundary_request, b"tenant:needle:000000");
    exchange_one(server, &boundary_request, b":3\r\n");
    exchange_one(server, SELECTIVE_PREFIX_PTTL, PTTL_PERSISTENT_REPLY);
    match workload {
        Workload::KeysSelectivePrefix => {
            exchange_one(server, KEYS_SELECTIVE_PREFIX, KEYS_SELECTIVE_PREFIX_REPLY);
        }
        Workload::ScanSelectivePrefix => {
            exchange_one(server, SCAN_SELECTIVE_PREFIX, SCAN_SELECTIVE_PREFIX_REPLY);
        }
        _ => unreachable!("selective-prefix fixture is only used by KEYS and SCAN"),
    }
}

fn prefill(servers: &mut [Server; 4], workload: Workload) {
    let seeded_stream = matches!(
        workload,
        Workload::XtrimMinidNoop
            | Workload::XdelMissing
            | Workload::XackMissing
            | Workload::XclaimMissing
            | Workload::XrangeZero
            | Workload::XrevrangeZero
            | Workload::XpendingZero
            | Workload::XreadAfterTail
    )
    .then(seeded_stream_prefill);
    for server in servers.iter_mut() {
        exchange_one(server, SET, SET_REPLY);
        if matches!(workload, Workload::BitposRange) {
            exchange_one(server, BITPOS_RANGE_PREFILL, SET_REPLY);
        } else if matches!(workload, Workload::BitfieldRoTwoGet) {
            exchange_one(server, BITFIELD_RO_TWO_GET_PREFILL, SET_REPLY);
        } else if matches!(workload, Workload::ObjectEncoding) {
            exchange_one(server, OBJECT_ENCODING_PREFILL, SET_REPLY);
        } else if matches!(workload, Workload::ObjectRefcount) {
            exchange_one(server, OBJECT_REFCOUNT_PREFILL, SET_REPLY);
        } else if matches!(workload, Workload::ZremrangebyscoreInverted) {
            exchange_one(
                server,
                ZREMRANGEBYSCORE_INVERTED_PREFILL,
                ZREMRANGEBYSCORE_INVERTED_PREFILL_REPLY,
            );
        } else if matches!(
            workload,
            Workload::ZremrangebyrankNoop | Workload::ZremrangebylexNoop
        ) {
            prefill_zrangebylex_source(server);
            assert_lexz_skiplist_fixture(server);
            let (request, response, no_op_bounds) =
                if matches!(workload, Workload::ZremrangebyrankNoop) {
                    (
                        ZREMRANGEBYRANK_NOOP,
                        ZREMRANGEBYRANK_NOOP_REPLY,
                        "rank_start=2500,rank_stop=3500",
                    )
                } else {
                    (
                        ZREMRANGEBYLEX_NOOP,
                        ZREMRANGEBYLEX_NOOP_REPLY,
                        "lex_min=[02500,lex_max=[03500",
                    )
                };
            exchange_one(server, request, response);
            assert_lexz_skiplist_fixture(server);
            println!(
                "FIXTURE_REPRESENTATION workload={} arm={} members={} \
uniform_score=0 member_width_bytes=5 no_op_bounds={no_op_bounds} \
removed=0 encoding=skiplist pttl=-1 cardinality_before_after=2000 \
steady_state_range=warmed_by_exact_assertion",
                workload.name(),
                server.arm.name(),
                ZRANGEBYLEX_MEMBERS
            );
        } else if matches!(
            workload,
            Workload::ZrangebyscoreRange
                | Workload::ZrevrangebyscoreRange
                | Workload::ZrevrangeRankRange
        ) {
            prefill_zrangebyscore_source(server);
            assert_scorez_skiplist_fixture(server);
            let (request, response, range_order, range_selector) = match workload {
                Workload::ZrangebyscoreRange => (
                    ZRANGEBYSCORE_RANGE,
                    zrangebyscore_range_reply(),
                    "ascending",
                    "inclusive_score_min=500,inclusive_score_max=1500",
                ),
                Workload::ZrevrangebyscoreRange => (
                    ZREVRANGEBYSCORE_RANGE,
                    zrevrangebyscore_range_reply(),
                    "descending",
                    "inclusive_score_min=500,inclusive_score_max=1500",
                ),
                Workload::ZrevrangeRankRange => (
                    ZREVRANGE_RANK_RANGE,
                    zrevrange_rank_range_reply(),
                    "descending",
                    "inclusive_reverse_rank_start=499,inclusive_reverse_rank_stop=1499",
                ),
                _ => unreachable!("scorez fixture is restricted to three range workloads"),
            };
            exchange_one(server, request, &response);
            println!(
                "FIXTURE_REPRESENTATION workload={} arm={} members={} \
distinct_integer_scores=0..1999 member_width_bytes=5 range_start={} range_end={} \
range_members={} encoding=skiplist pttl=-1 full_response_bytes_asserted=true \
range_order={range_order} range_selector={range_selector} \
steady_state_range=warmed_by_exact_assertion",
                workload.name(),
                server.arm.name(),
                ZRANGEBYSCORE_MEMBERS,
                ZRANGEBYSCORE_RANGE_START,
                ZRANGEBYSCORE_RANGE_END,
                ZRANGEBYSCORE_RANGE_END - ZRANGEBYSCORE_RANGE_START + 1
            );
        } else if matches!(
            workload,
            Workload::ZrangebylexRange | Workload::ZrevrangebylexRange
        ) {
            prefill_zrangebylex_source(server);
            assert_lexz_skiplist_fixture(server);
            let (request, response, range_order) = if matches!(workload, Workload::ZrangebylexRange)
            {
                (ZRANGEBYLEX_RANGE, zrangebylex_range_reply(), "ascending")
            } else {
                (
                    ZREVRANGEBYLEX_RANGE,
                    zrevrangebylex_range_reply(),
                    "descending",
                )
            };
            exchange_one(server, request, &response);
            println!(
                "FIXTURE_REPRESENTATION workload={} arm={} members={} \
uniform_score=0 member_width_bytes=5 range_start={} range_end={} \
range_members={} encoding=skiplist pttl=-1 full_response_bytes_asserted=true \
range_order={range_order} steady_state_range=warmed_by_exact_assertion",
                workload.name(),
                server.arm.name(),
                ZRANGEBYLEX_MEMBERS,
                ZRANGEBYLEX_RANGE_START,
                ZRANGEBYLEX_RANGE_END,
                ZRANGEBYLEX_RANGE_END - ZRANGEBYLEX_RANGE_START + 1
            );
        } else if matches!(workload, Workload::LrangeInverted) {
            exchange_one(
                server,
                LRANGE_INVERTED_PREFILL,
                LRANGE_INVERTED_PREFILL_REPLY,
            );
        } else if matches!(
            workload,
            Workload::LpopCountMissing | Workload::RpopCountMissing
        ) {
            let request = if matches!(workload, Workload::LpopCountMissing) {
                LPOP_COUNT_MISSING
            } else {
                RPOP_COUNT_MISSING
            };
            exchange_one(server, request, LIST_COUNT_MISSING_REPLY);
            exchange_one(
                server,
                LIST_COUNT_MISSING_EXISTS,
                LIST_COUNT_MISSING_EXISTS_REPLY,
            );
            exchange_one(
                server,
                LIST_COUNT_MISSING_PTTL,
                LIST_COUNT_MISSING_PTTL_REPLY,
            );
            println!(
                "FIXTURE_REPRESENTATION workload={} arm={} key=missing-list \
count=2 key_exists=false pttl=-2 exact_null_array_reply=true \
steady_state=missing_key_warmed_by_exact_assertion",
                workload.name(),
                server.arm.name()
            );
        } else if matches!(
            workload,
            Workload::LindexMiddle | Workload::LsetMiddleSameValue | Workload::LposMiddleElement
        ) {
            let case = lindex_middle_prefill();
            exchange_one(server, &case.request, &case.response);
            exchange_one(server, LINDEX_MIDDLE_LLEN, LINDEX_MIDDLE_LLEN_REPLY);
            if matches!(server.arm, Arm::Redis) {
                exchange_one(
                    server,
                    LINDEX_MIDDLE_ENCODING,
                    LINDEX_MIDDLE_REDIS_ENCODING_REPLY,
                );
                exchange_one(
                    server,
                    LINDEX_MIDDLE_CONFIG,
                    LINDEX_MIDDLE_REDIS_CONFIG_REPLY,
                );
                println!(
                    "FIXTURE_REPRESENTATION workload={} arm=redis elements={} \
element_bytes=4 encoding=listpack list_max_listpack_size=-2 \
derived_listpack_bytes=3007",
                    workload.name(),
                    LINDEX_MIDDLE_ELEMENTS
                );
            }
        } else if matches!(
            workload,
            Workload::SscanCursorZero | Workload::HscanCursorZero | Workload::ZscanCursorZero
        ) {
            let case = scan0_prefill(workload);
            exchange_one(server, &case.request, &case.response);
            assert_scan0_fixture(server, workload);
            let (request, response) = scan0_request_reply(workload);
            exchange_one(server, request, &response);
            let compact_encoding = if matches!(workload, Workload::SscanCursorZero) {
                "intset"
            } else {
                "listpack"
            };
            println!(
                "FIXTURE_REPRESENTATION workload={} arm={} members={} \
literal_cursor=0 options=none default_count=10 returned_cursor=0 \
encoding={compact_encoding} pttl=-1 boundary_membership=verified \
full_response_bytes_asserted=true steady_state_scan=warmed_by_two_exact_assertions",
                workload.name(),
                server.arm.name(),
                SCAN0_MEMBERS
            );
        } else if matches!(
            workload,
            Workload::HdelMissingField
                | Workload::HgetMissingField
                | Workload::HexistsMissingField
                | Workload::HkeysFields
                | Workload::HvalsFields
                | Workload::HgetallFields
                | Workload::HscanAllFields
                | Workload::HsetSameValue
                | Workload::HsetnxExistingField
                | Workload::HincrbyZeroDelta
                | Workload::HincrbyfloatZeroDelta
                | Workload::HstrlenExistingField
                | Workload::HmgetExistingMissing
        ) {
            let value = if matches!(
                workload,
                Workload::HkeysFields
                    | Workload::HvalsFields
                    | Workload::HgetallFields
                    | Workload::HscanAllFields
                    | Workload::HsetSameValue
                    | Workload::HincrbyZeroDelta
                    | Workload::HincrbyfloatZeroDelta
            ) {
                "1"
            } else {
                "v"
            };
            let case = hash_prefill(value);
            exchange_one(server, &case.request, &case.response);
            exchange_one(
                server,
                MISSING_FIELD_HASH_HLEN,
                MISSING_FIELD_HASH_HLEN_REPLY,
            );
            if matches!(server.arm, Arm::Redis) {
                exchange_one(
                    server,
                    MISSING_FIELD_HASH_ENCODING,
                    MISSING_FIELD_HASH_REDIS_ENCODING_REPLY,
                );
                println!(
                    "FIXTURE_REPRESENTATION workload={} arm=redis fields={} encoding=listpack",
                    workload.name(),
                    MISSING_FIELD_HASH_FIELDS
                );
            }
        } else if matches!(
            workload,
            Workload::PfmergeTwoDense | Workload::PfcountTwoDense
        ) {
            prefill_two_dense_hll_sources(server);
            exchange_one(server, PFMERGE_H1_ENCODING, PFMERGE_DENSE_ENCODING_REPLY);
            exchange_one(server, PFMERGE_H2_ENCODING, PFMERGE_DENSE_ENCODING_REPLY);
            if matches!(workload, Workload::PfmergeTwoDense) {
                exchange_one(server, PFMERGE_TWO_DENSE, PFMERGE_TWO_DENSE_REPLY);
                exchange_one(server, PFMERGE_DST_ENCODING, PFMERGE_DENSE_ENCODING_REPLY);
                exchange_one(server, PFMERGE_DST_COUNT, PFMERGE_DST_COUNT_REPLY);
                println!(
                    "FIXTURE_REPRESENTATION workload={} arm={} sources=2 \
elements_per_source={} source_encoding=dense destination_encoding=dense \
union_count=8173",
                    workload.name(),
                    server.arm.name(),
                    PFMERGE_DENSE_SOURCE_ELEMENTS
                );
            } else {
                exchange_one(server, PFCOUNT_TWO_DENSE, PFCOUNT_TWO_DENSE_REPLY);
                println!(
                    "FIXTURE_REPRESENTATION workload={} arm={} sources=2 \
elements_per_source={} source_encoding=dense union_count=8173 \
steady_state_register_cache=warmed_by_exact_assertion",
                    workload.name(),
                    server.arm.name(),
                    PFMERGE_DENSE_SOURCE_ELEMENTS
                );
            }
        } else if matches!(workload, Workload::BitcountOneMib) {
            let case = bitcount_one_mib_prefill();
            exchange_one(server, &case.request, &case.response);
            exchange_one(
                server,
                BITCOUNT_ONE_MIB_STRLEN,
                BITCOUNT_ONE_MIB_STRLEN_REPLY,
            );
            exchange_one(
                server,
                BITCOUNT_ONE_MIB_ENCODING,
                BITCOUNT_ONE_MIB_ENCODING_REPLY,
            );
            let get_reply = bitcount_one_mib_get_reply();
            exchange_one(server, BITCOUNT_ONE_MIB_GET, &get_reply);
            exchange_one(server, BITCOUNT_ONE_MIB, BITCOUNT_ONE_MIB_REPLY);
            println!(
                "FIXTURE_REPRESENTATION workload={} arm={} bytes={} byte_pattern=0xaa \
encoding=raw exact_bitcount=4194304 full_get_byte_identity=verified \
steady_state_cache=warmed_by_exact_assertion",
                workload.name(),
                server.arm.name(),
                BITCOUNT_ONE_MIB_BYTES
            );
        } else if matches!(
            workload,
            Workload::KeysSelectivePrefix | Workload::ScanSelectivePrefix
        ) {
            prefill_selective_prefix_keyspace(server, workload);
            println!(
                "FIXTURE_REPRESENTATION workload={} arm={} \
total_keys={} decoy_keys={} literal_prefix_candidates=1 exact_reply_keys=1 \
matched_key_pttl=-1 boundary_membership=verified \
steady_state_ordered_index=warmed_by_exact_assertion",
                workload.name(),
                server.arm.name(),
                SELECTIVE_PREFIX_DECOYS + 2,
                SELECTIVE_PREFIX_DECOYS
            );
        } else if matches!(
            workload,
            Workload::ZintercardCached | Workload::ZdiffTwoKey | Workload::ZinterTwoKey
        ) {
            prefill_zintercard_sources(server);
            exchange_one(server, ZINTERCARD_ZA_CARD, ZINTERCARD_SOURCE_CARD_REPLY);
            exchange_one(server, ZINTERCARD_ZB_CARD, ZINTERCARD_SOURCE_CARD_REPLY);
            exchange_one(
                server,
                ZINTERCARD_ZA_ENCODING,
                ZINTERCARD_SKIPLIST_ENCODING_REPLY,
            );
            exchange_one(
                server,
                ZINTERCARD_ZB_ENCODING,
                ZINTERCARD_SKIPLIST_ENCODING_REPLY,
            );
            exchange_one(
                server,
                ZINTERCARD_ZA_BOUNDARIES,
                ZINTERCARD_ZA_BOUNDARIES_REPLY,
            );
            exchange_one(
                server,
                ZINTERCARD_ZB_BOUNDARIES,
                ZINTERCARD_ZB_BOUNDARIES_REPLY,
            );
            exchange_one(server, ZINTERCARD_ZA_PTTL, PTTL_PERSISTENT_REPLY);
            exchange_one(server, ZINTERCARD_ZB_PTTL, PTTL_PERSISTENT_REPLY);
            let (request, response, result_shape) = match workload {
                Workload::ZintercardCached => (
                    ZINTERCARD_CACHED,
                    ZINTERCARD_CACHED_REPLY.to_vec(),
                    "intersection_cardinality=2048",
                ),
                Workload::ZdiffTwoKey => (
                    ZDIFF_TWO_KEY,
                    zdiff_two_key_reply(),
                    "difference_members=0..2047",
                ),
                Workload::ZinterTwoKey => (
                    ZINTER_TWO_KEY,
                    zinter_two_key_reply(),
                    "intersection_members=2048..4095",
                ),
                _ => unreachable!("zset algebra fixture is restricted to three workloads"),
            };
            exchange_one(server, request, &response);
            exchange_one(server, request, &response);
            println!(
                "FIXTURE_REPRESENTATION workload={} arm={} \
source_a_members={} source_a_range=0..4095 source_a_encoding=skiplist \
source_b_members={} source_b_range=2048..6143 source_b_encoding=skiplist \
intersection_members={} source_pttl=-1 boundary_scores=verified \
result_shape={result_shape} full_response_bytes_asserted=true \
steady_state_result=warmed_by_two_exact_assertions",
                workload.name(),
                server.arm.name(),
                ZINTERCARD_SOURCE_MEMBERS,
                ZINTERCARD_SOURCE_MEMBERS,
                ZINTERCARD_INTERSECTION_MEMBERS
            );
        } else if matches!(
            workload,
            Workload::SunionstoreMixed | Workload::SdiffstoreMixed | Workload::SinterstoreMixed
        ) {
            let (large_key, large_start) = if matches!(workload, Workload::SinterstoreMixed) {
                ("large", 0)
            } else {
                ("large_miss", SUNIONSTORE_LARGE_START)
            };
            prefill_mixed_setstore_sources(server, large_key, large_start);
            exchange_one(
                server,
                SUNIONSTORE_SMALL_SCARD,
                SUNIONSTORE_SMALL_SCARD_REPLY,
            );
            if matches!(workload, Workload::SinterstoreMixed) {
                exchange_one(
                    server,
                    SINTERSTORE_LARGE_SCARD,
                    SUNIONSTORE_LARGE_SCARD_REPLY,
                );
            } else {
                exchange_one(
                    server,
                    SUNIONSTORE_LARGE_SCARD,
                    SUNIONSTORE_LARGE_SCARD_REPLY,
                );
            }
            exchange_one(
                server,
                SUNIONSTORE_SMALL_ENCODING,
                SUNIONSTORE_SMALL_ENCODING_REPLY,
            );
            if matches!(workload, Workload::SinterstoreMixed) {
                exchange_one(
                    server,
                    SINTERSTORE_LARGE_ENCODING,
                    SUNIONSTORE_HASHTABLE_ENCODING_REPLY,
                );
            } else {
                exchange_one(
                    server,
                    SUNIONSTORE_LARGE_ENCODING,
                    SUNIONSTORE_HASHTABLE_ENCODING_REPLY,
                );
            }
            if matches!(workload, Workload::SunionstoreMixed) {
                exchange_one(server, SUNIONSTORE_MIXED, SUNIONSTORE_MIXED_REPLY);
                exchange_one(server, SUNIONSTORE_DST_SCARD, SUNIONSTORE_DST_SCARD_REPLY);
                exchange_one(
                    server,
                    SUNIONSTORE_DST_ENCODING,
                    SUNIONSTORE_HASHTABLE_ENCODING_REPLY,
                );
                exchange_one(
                    server,
                    SUNIONSTORE_DST_MEMBERSHIP,
                    SUNIONSTORE_DST_MEMBERSHIP_REPLY,
                );
                exchange_one(server, SUNIONSTORE_DST_PTTL, PTTL_PERSISTENT_REPLY);
                println!(
                    "FIXTURE_REPRESENTATION workload={} arm={} \
source_small_members={} source_small_encoding=intset \
source_large_members={} source_large_encoding=hashtable disjoint=true \
destination_members=4608 destination_encoding=hashtable \
destination_pttl=-1 boundary_membership=verified \
steady_state_destination=warmed_by_exact_assertion",
                    workload.name(),
                    server.arm.name(),
                    SUNIONSTORE_SMALL_MEMBERS,
                    SUNIONSTORE_LARGE_MEMBERS
                );
            } else if matches!(workload, Workload::SdiffstoreMixed) {
                exchange_one(server, SDIFFSTORE_MIXED, SDIFFSTORE_MIXED_REPLY);
                exchange_one(server, SUNIONSTORE_DST_SCARD, SDIFFSTORE_DST_SCARD_REPLY);
                exchange_one(
                    server,
                    SUNIONSTORE_DST_ENCODING,
                    SUNIONSTORE_SMALL_ENCODING_REPLY,
                );
                exchange_one(
                    server,
                    SDIFFSTORE_DST_MEMBERSHIP,
                    SDIFFSTORE_DST_MEMBERSHIP_REPLY,
                );
                exchange_one(server, SUNIONSTORE_DST_PTTL, PTTL_PERSISTENT_REPLY);
                println!(
                    "FIXTURE_REPRESENTATION workload={} arm={} \
source_small_members={} source_small_encoding=intset \
source_large_members={} source_large_encoding=hashtable disjoint=true \
destination_members=512 destination_encoding=intset \
destination_pttl=-1 boundary_membership=verified \
steady_state_destination=warmed_by_exact_assertion",
                    workload.name(),
                    server.arm.name(),
                    SUNIONSTORE_SMALL_MEMBERS,
                    SUNIONSTORE_LARGE_MEMBERS
                );
            } else {
                exchange_one(server, SINTERSTORE_MIXED, SINTERSTORE_MIXED_REPLY);
                exchange_one(server, SUNIONSTORE_DST_SCARD, SDIFFSTORE_DST_SCARD_REPLY);
                exchange_one(
                    server,
                    SUNIONSTORE_DST_ENCODING,
                    SUNIONSTORE_SMALL_ENCODING_REPLY,
                );
                exchange_one(
                    server,
                    SINTERSTORE_DST_MEMBERSHIP,
                    SINTERSTORE_DST_MEMBERSHIP_REPLY,
                );
                exchange_one(server, SUNIONSTORE_DST_PTTL, PTTL_PERSISTENT_REPLY);
                println!(
                    "FIXTURE_REPRESENTATION workload={} arm={} \
source_small_members={} source_small_encoding=intset \
source_large_members={} source_large_encoding=hashtable fully_contains_small=true \
destination_members=512 destination_encoding=intset \
destination_pttl=-1 boundary_membership=verified \
steady_state_destination=warmed_by_exact_assertion",
                    workload.name(),
                    server.arm.name(),
                    SUNIONSTORE_SMALL_MEMBERS,
                    SUNIONSTORE_LARGE_MEMBERS
                );
            }
        } else if let Some(case) = &seeded_stream {
            exchange_one(server, &case.request, &case.response);
        }
        if matches!(
            workload,
            Workload::XackMissing | Workload::XclaimMissing | Workload::XpendingZero
        ) {
            exchange_one(server, XGROUP_CREATE_XS_G, SET_REPLY);
        }
        if matches!(workload, Workload::XclaimMissing) {
            exchange_one(server, XGROUP_CREATECONSUMER_XS_G_C, b":1\r\n");
        }
        if matches!(workload, Workload::XpendingZero) {
            let case = seeded_pending_prefill();
            exchange_one(server, &case.request, &case.response);
        }
    }
}

fn prefill_and_warm(
    servers: &mut [Server; 4],
    workload: Workload,
    pipeline: usize,
    clients: usize,
    packets: &Arc<DriverPackets>,
) {
    prefill(servers, workload);
    let warm_ops: usize = if matches!(
        workload,
        Workload::KeysSelectivePrefix | Workload::ScanSelectivePrefix
    ) {
        1_600
    } else if matches!(
        workload,
        Workload::SunionstoreMixed
            | Workload::SdiffstoreMixed
            | Workload::SinterstoreMixed
            | Workload::ZintercardCached
            | Workload::ZrangebyscoreRange
            | Workload::ZrevrangebyscoreRange
            | Workload::ZrevrangeRankRange
            | Workload::ZrangebylexRange
            | Workload::ZrevrangebylexRange
    ) {
        3_200
    } else {
        20_000
    };
    let warm_groups = if matches!(
        workload,
        Workload::KeysSelectivePrefix | Workload::ScanSelectivePrefix
    ) {
        warm_ops.div_ceil(clients * pipeline).max(2)
    } else {
        warm_ops.div_ceil(clients * pipeline).max(8)
    };
    for arm in Arm::ALL {
        time_block(
            &mut servers[arm.index()],
            packets,
            warm_groups,
            matches!(workload, Workload::Mixed),
        );
    }
}

#[derive(Debug)]
struct Sample {
    mio_a_ns: f64,
    mio_b_ns: f64,
    io_uring_ns: f64,
    redis_ns: f64,
    null_ratio: f64,
    self_speedup: f64,
    competitive_speedup: f64,
    mio_a_cpu_ns: u64,
    mio_b_cpu_ns: u64,
    io_uring_cpu_ns: u64,
    redis_cpu_ns: u64,
    io_uring_cpu_util_pct: f64,
    redis_cpu_util_pct: f64,
    cpu_null_ratio: f64,
    cpu_self_speedup: f64,
    cpu_competitive_speedup: f64,
}

#[derive(Debug)]
struct DualNullSample {
    candidate_a_ns: f64,
    candidate_b_ns: f64,
    redis_a_ns: f64,
    redis_b_ns: f64,
    candidate_null_ratio: f64,
    redis_null_ratio: f64,
    competitive_speedup: f64,
    candidate_a_cpu_ns: u64,
    candidate_b_cpu_ns: u64,
    redis_a_cpu_ns: u64,
    redis_b_cpu_ns: u64,
    candidate_a_cpu_util_pct: f64,
    candidate_b_cpu_util_pct: f64,
    redis_a_cpu_util_pct: f64,
    redis_b_cpu_util_pct: f64,
    cpu_candidate_null_ratio: f64,
    cpu_redis_null_ratio: f64,
    cpu_competitive_speedup: f64,
}

#[derive(Clone, Copy, Debug)]
enum DualNullArm {
    CandidateA,
    CandidateB,
    RedisA,
    RedisB,
}

impl DualNullArm {
    const fn index(self) -> usize {
        match self {
            Self::CandidateA => 0,
            Self::CandidateB => 1,
            Self::RedisA => 2,
            Self::RedisB => 3,
        }
    }
}

#[derive(Clone, Copy)]
struct MeasurementConfig {
    client_shape: ClientShape,
    samples: usize,
    ops_per_sample: usize,
    interleave_groups: usize,
}

#[derive(Clone, Copy)]
struct PacketMeasurement<'a> {
    prepare_keyed: bool,
    workload_shape: &'a str,
    host_wide_allowed_cpus: Option<&'a [usize]>,
}

fn measure_configuration(
    servers: &mut [Server; 4],
    workload: Workload,
    pipeline: usize,
    config: MeasurementConfig,
    host_wide_allowed_cpus: &[usize],
) -> Vec<Sample> {
    measure_configuration_with_packets(
        servers,
        workload,
        pipeline,
        config,
        Arc::new(DriverPackets::shared(workload, pipeline)),
        PacketMeasurement {
            prepare_keyed: false,
            workload_shape: "shared_hot_key",
            host_wide_allowed_cpus: Some(host_wide_allowed_cpus),
        },
    )
}

fn measure_configuration_with_packets(
    servers: &mut [Server; 4],
    workload: Workload,
    pipeline: usize,
    config: MeasurementConfig,
    packets: Arc<DriverPackets>,
    packet_measurement: PacketMeasurement<'_>,
) -> Vec<Sample> {
    let PacketMeasurement {
        prepare_keyed,
        workload_shape,
        host_wide_allowed_cpus,
    } = packet_measurement;
    let ClientShape {
        connections: clients,
        driver_threads: client_threads,
    } = config.client_shape;
    let MeasurementConfig {
        samples,
        ops_per_sample,
        interleave_groups,
        ..
    } = config;
    // The 24 permutations rotate across samples. Within a sample, each arm runs
    // only `interleave_groups` client groups before control passes to the next arm,
    // so host-frequency and queue drift cannot alias onto a multi-second block.
    const ORDERS: [[Arm; 4]; 24] = [
        [Arm::MioA, Arm::MioB, Arm::IoUring, Arm::Redis],
        [Arm::MioA, Arm::MioB, Arm::Redis, Arm::IoUring],
        [Arm::MioA, Arm::IoUring, Arm::MioB, Arm::Redis],
        [Arm::MioA, Arm::IoUring, Arm::Redis, Arm::MioB],
        [Arm::MioA, Arm::Redis, Arm::MioB, Arm::IoUring],
        [Arm::MioA, Arm::Redis, Arm::IoUring, Arm::MioB],
        [Arm::MioB, Arm::MioA, Arm::IoUring, Arm::Redis],
        [Arm::MioB, Arm::MioA, Arm::Redis, Arm::IoUring],
        [Arm::MioB, Arm::IoUring, Arm::MioA, Arm::Redis],
        [Arm::MioB, Arm::IoUring, Arm::Redis, Arm::MioA],
        [Arm::MioB, Arm::Redis, Arm::MioA, Arm::IoUring],
        [Arm::MioB, Arm::Redis, Arm::IoUring, Arm::MioA],
        [Arm::IoUring, Arm::MioA, Arm::MioB, Arm::Redis],
        [Arm::IoUring, Arm::MioA, Arm::Redis, Arm::MioB],
        [Arm::IoUring, Arm::MioB, Arm::MioA, Arm::Redis],
        [Arm::IoUring, Arm::MioB, Arm::Redis, Arm::MioA],
        [Arm::IoUring, Arm::Redis, Arm::MioA, Arm::MioB],
        [Arm::IoUring, Arm::Redis, Arm::MioB, Arm::MioA],
        [Arm::Redis, Arm::MioA, Arm::MioB, Arm::IoUring],
        [Arm::Redis, Arm::MioA, Arm::IoUring, Arm::MioB],
        [Arm::Redis, Arm::MioB, Arm::MioA, Arm::IoUring],
        [Arm::Redis, Arm::MioB, Arm::IoUring, Arm::MioA],
        [Arm::Redis, Arm::IoUring, Arm::MioA, Arm::MioB],
        [Arm::Redis, Arm::IoUring, Arm::MioB, Arm::MioA],
    ];
    assert!(
        samples.is_multiple_of(ORDERS.len()),
        "sample count must contain complete 24-order cycles; got {samples}"
    );
    assert!(
        interleave_groups > 0,
        "interleave group count must be positive"
    );

    if prepare_keyed {
        for arm in Arm::ALL {
            servers[arm.index()]
                .clients
                .as_ref()
                .expect("benchmark clients initialized")
                .prepare(&packets);
        }
        let warm_groups = 20_000usize.div_ceil(clients * pipeline).max(8);
        for arm in Arm::ALL {
            time_block(
                &mut servers[arm.index()],
                &packets,
                warm_groups,
                matches!(workload, Workload::Mixed),
            );
        }
    } else {
        prefill_and_warm(servers, workload, pipeline, clients, &packets);
    }
    if let Some(allowed_cpus) = host_wide_allowed_cpus {
        assert_host_wide_quiescence(
            allowed_cpus,
            &format!("before_{}_{}", workload.name(), workload_shape),
        );
    }
    let groups = ops_per_sample.div_ceil(clients * pipeline).max(1);
    let actual_ops = groups * clients * pipeline;
    let mut output = Vec::with_capacity(samples);

    println!(
        "CONFIG workload={} workload_shape={workload_shape} pipeline={pipeline} \
clients={clients} client_threads={client_threads} \
samples={samples} \
groups_per_arm_sample={groups} interleave_groups={interleave_groups} \
ops_per_arm_sample={actual_ops}",
        workload.name()
    );
    for sample_index in 0..samples {
        // The two mio processes are byte-identical controls, but fixed logical
        // labels let a persistent process-instance bias shift the A/A median.
        // Swap their identities every sample and use an even sample count, so
        // each physical process contributes equally to both sides of the null.
        let swap_controls = sample_index % 2 == 1;
        let mio_a_slot = usize::from(swap_controls);
        let mio_b_slot = usize::from(!swap_controls);
        let mut elapsed = [Duration::ZERO; 4];
        let mut cpu_elapsed = [0_u64; 4];
        let mut groups_done = 0usize;
        let mut interleave_index = 0usize;
        while groups_done < groups {
            let block_groups = (groups - groups_done).min(interleave_groups);
            let order = ORDERS[(sample_index + interleave_index) % ORDERS.len()];
            for arm in order {
                let server_slot = match arm {
                    Arm::MioA => mio_a_slot,
                    Arm::MioB => mio_b_slot,
                    Arm::IoUring => Arm::IoUring.index(),
                    Arm::Redis => Arm::Redis.index(),
                };
                let cpu_before = servers[server_slot].cpu_ns();
                let block_elapsed = time_block(
                    &mut servers[server_slot],
                    &packets,
                    block_groups,
                    (groups_done % 2 == 1) ^ (sample_index % 2 == 1),
                );
                let cpu_after = servers[server_slot].cpu_ns();
                elapsed[arm.index()] += block_elapsed;
                cpu_elapsed[arm.index()] += cpu_after - cpu_before;
            }
            groups_done += block_groups;
            interleave_index += 1;
        }
        let mio_a_cpu_ns = cpu_elapsed[Arm::MioA.index()];
        let mio_b_cpu_ns = cpu_elapsed[Arm::MioB.index()];
        let io_uring_cpu_ns = cpu_elapsed[Arm::IoUring.index()];
        let redis_cpu_ns = cpu_elapsed[Arm::Redis.index()];
        assert!(
            mio_a_cpu_ns > 0 && mio_b_cpu_ns > 0 && io_uring_cpu_ns > 0 && redis_cpu_ns > 0,
            "each server arm must accrue CPU time"
        );
        let mio_a_ns = elapsed[Arm::MioA.index()].as_nanos() as f64;
        let mio_b_ns = elapsed[Arm::MioB.index()].as_nanos() as f64;
        let io_uring_ns = elapsed[Arm::IoUring.index()].as_nanos() as f64;
        let redis_ns = elapsed[Arm::Redis.index()].as_nanos() as f64;
        let mio_center_ns = (mio_a_ns * mio_b_ns).sqrt();
        let result = Sample {
            mio_a_ns,
            mio_b_ns,
            io_uring_ns,
            redis_ns,
            null_ratio: mio_a_ns / mio_b_ns,
            self_speedup: mio_center_ns / io_uring_ns,
            competitive_speedup: redis_ns / io_uring_ns,
            mio_a_cpu_ns,
            mio_b_cpu_ns,
            io_uring_cpu_ns,
            redis_cpu_ns,
            io_uring_cpu_util_pct: io_uring_cpu_ns as f64 / io_uring_ns * 100.0,
            redis_cpu_util_pct: redis_cpu_ns as f64 / redis_ns * 100.0,
            cpu_null_ratio: mio_a_cpu_ns as f64 / mio_b_cpu_ns as f64,
            cpu_self_speedup: (mio_a_cpu_ns as f64 * mio_b_cpu_ns as f64).sqrt()
                / io_uring_cpu_ns as f64,
            cpu_competitive_speedup: redis_cpu_ns as f64 / io_uring_cpu_ns as f64,
        };
        println!(
            "SAMPLE workload={} workload_shape={workload_shape} pipeline={pipeline} \
client_driver_threads={client_threads} \
sample={} order={:?} \
control_slots={} \
control_a_ns_per_op={:.3} control_b_ns_per_op={:.3} candidate_ns_per_op={:.3} \
redis_ns_per_op={:.3} null_control_a_over_b={:.9} \
control_geomean_over_candidate={:.9} candidate_over_redis={:.9} \
control_a_cpu_ns={} control_b_cpu_ns={} candidate_cpu_ns={} redis_cpu_ns={} \
candidate_cpu_util_pct={:.3} redis_cpu_util_pct={:.3} \
cpu_null_control_a_over_b={:.9} cpu_control_geomean_over_candidate={:.9} \
cpu_candidate_over_redis={:.9}",
            workload.name(),
            sample_index + 1,
            ORDERS[sample_index % ORDERS.len()],
            if swap_controls { "BA" } else { "AB" },
            result.mio_a_ns / actual_ops as f64,
            result.mio_b_ns / actual_ops as f64,
            result.io_uring_ns / actual_ops as f64,
            result.redis_ns / actual_ops as f64,
            result.null_ratio,
            result.self_speedup,
            result.competitive_speedup,
            result.mio_a_cpu_ns,
            result.mio_b_cpu_ns,
            result.io_uring_cpu_ns,
            result.redis_cpu_ns,
            result.io_uring_cpu_util_pct,
            result.redis_cpu_util_pct,
            result.cpu_null_ratio,
            result.cpu_self_speedup,
            result.cpu_competitive_speedup,
        );
        output.push(result);
    }
    if let Some(allowed_cpus) = host_wide_allowed_cpus {
        assert_host_wide_quiescence(
            allowed_cpus,
            &format!("after_{}_{}", workload.name(), workload_shape),
        );
    }
    output
}

fn measure_dual_null_configuration_with_packets(
    servers: &mut [Server; 4],
    workload: Workload,
    pipeline: usize,
    config: MeasurementConfig,
    packets: Arc<DriverPackets>,
    packet_measurement: PacketMeasurement<'_>,
) -> Vec<DualNullSample> {
    let PacketMeasurement {
        prepare_keyed,
        workload_shape,
        host_wide_allowed_cpus,
    } = packet_measurement;
    assert!(
        prepare_keyed || packets.connections.len() == 1,
        "a dual-null fixture without keyed preparation must use one shared packet oracle"
    );
    let ClientShape {
        connections: clients,
        driver_threads: client_threads,
    } = config.client_shape;
    let MeasurementConfig {
        samples,
        ops_per_sample,
        interleave_groups,
        ..
    } = config;
    const ORDERS: [[DualNullArm; 4]; 24] = [
        [
            DualNullArm::CandidateA,
            DualNullArm::CandidateB,
            DualNullArm::RedisA,
            DualNullArm::RedisB,
        ],
        [
            DualNullArm::CandidateA,
            DualNullArm::CandidateB,
            DualNullArm::RedisB,
            DualNullArm::RedisA,
        ],
        [
            DualNullArm::CandidateA,
            DualNullArm::RedisA,
            DualNullArm::CandidateB,
            DualNullArm::RedisB,
        ],
        [
            DualNullArm::CandidateA,
            DualNullArm::RedisA,
            DualNullArm::RedisB,
            DualNullArm::CandidateB,
        ],
        [
            DualNullArm::CandidateA,
            DualNullArm::RedisB,
            DualNullArm::CandidateB,
            DualNullArm::RedisA,
        ],
        [
            DualNullArm::CandidateA,
            DualNullArm::RedisB,
            DualNullArm::RedisA,
            DualNullArm::CandidateB,
        ],
        [
            DualNullArm::CandidateB,
            DualNullArm::CandidateA,
            DualNullArm::RedisA,
            DualNullArm::RedisB,
        ],
        [
            DualNullArm::CandidateB,
            DualNullArm::CandidateA,
            DualNullArm::RedisB,
            DualNullArm::RedisA,
        ],
        [
            DualNullArm::CandidateB,
            DualNullArm::RedisA,
            DualNullArm::CandidateA,
            DualNullArm::RedisB,
        ],
        [
            DualNullArm::CandidateB,
            DualNullArm::RedisA,
            DualNullArm::RedisB,
            DualNullArm::CandidateA,
        ],
        [
            DualNullArm::CandidateB,
            DualNullArm::RedisB,
            DualNullArm::CandidateA,
            DualNullArm::RedisA,
        ],
        [
            DualNullArm::CandidateB,
            DualNullArm::RedisB,
            DualNullArm::RedisA,
            DualNullArm::CandidateA,
        ],
        [
            DualNullArm::RedisA,
            DualNullArm::CandidateA,
            DualNullArm::CandidateB,
            DualNullArm::RedisB,
        ],
        [
            DualNullArm::RedisA,
            DualNullArm::CandidateA,
            DualNullArm::RedisB,
            DualNullArm::CandidateB,
        ],
        [
            DualNullArm::RedisA,
            DualNullArm::CandidateB,
            DualNullArm::CandidateA,
            DualNullArm::RedisB,
        ],
        [
            DualNullArm::RedisA,
            DualNullArm::CandidateB,
            DualNullArm::RedisB,
            DualNullArm::CandidateA,
        ],
        [
            DualNullArm::RedisA,
            DualNullArm::RedisB,
            DualNullArm::CandidateA,
            DualNullArm::CandidateB,
        ],
        [
            DualNullArm::RedisA,
            DualNullArm::RedisB,
            DualNullArm::CandidateB,
            DualNullArm::CandidateA,
        ],
        [
            DualNullArm::RedisB,
            DualNullArm::CandidateA,
            DualNullArm::CandidateB,
            DualNullArm::RedisA,
        ],
        [
            DualNullArm::RedisB,
            DualNullArm::CandidateA,
            DualNullArm::RedisA,
            DualNullArm::CandidateB,
        ],
        [
            DualNullArm::RedisB,
            DualNullArm::CandidateB,
            DualNullArm::CandidateA,
            DualNullArm::RedisA,
        ],
        [
            DualNullArm::RedisB,
            DualNullArm::CandidateB,
            DualNullArm::RedisA,
            DualNullArm::CandidateA,
        ],
        [
            DualNullArm::RedisB,
            DualNullArm::RedisA,
            DualNullArm::CandidateA,
            DualNullArm::CandidateB,
        ],
        [
            DualNullArm::RedisB,
            DualNullArm::RedisA,
            DualNullArm::CandidateB,
            DualNullArm::CandidateA,
        ],
    ];
    assert!(
        samples.is_multiple_of(ORDERS.len()),
        "sample count must contain complete 24-order cycles; got {samples}"
    );
    assert!(
        interleave_groups > 0,
        "interleave group count must be positive"
    );

    for server in servers.iter() {
        server
            .clients
            .as_ref()
            .expect("benchmark clients initialized")
            .prepare(&packets);
    }
    let warm_ops: usize = if matches!(
        workload,
        Workload::ZrangebyscoreRange
            | Workload::ZrevrangebyscoreRange
            | Workload::ZrevrangeRankRange
            | Workload::ZrangebylexRange
            | Workload::ZrevrangebylexRange
    ) {
        3_200
    } else {
        20_000
    };
    let warm_groups = warm_ops.div_ceil(clients * pipeline).max(8);
    for server in servers.iter_mut() {
        time_block(
            server,
            &packets,
            warm_groups,
            matches!(workload, Workload::Mixed),
        );
    }
    if let Some(allowed_cpus) = host_wide_allowed_cpus {
        assert_host_wide_quiescence(
            allowed_cpus,
            &format!("before_dual_null_{}_{}", workload.name(), workload_shape),
        );
    }
    let groups = ops_per_sample.div_ceil(clients * pipeline).max(1);
    let actual_ops = groups * clients * pipeline;
    let mut output = Vec::with_capacity(samples);

    println!(
        "CONFIG_DUAL_NULL workload={} workload_shape={workload_shape} pipeline={pipeline} \
clients={clients} client_threads={client_threads} samples={samples} \
groups_per_arm_sample={groups} interleave_groups={interleave_groups} \
ops_per_arm_sample={actual_ops} semantic_commands_per_arm_sample={actual_ops} \
full_response_bytes_asserted=true",
        workload.name()
    );
    for sample_index in 0..samples {
        // Swap the two physical processes behind each logical A/A pair every
        // sample. Persistent process-instance bias therefore cannot shift
        // either candidate or incumbent null median.
        let swap_pairs = sample_index % 2 == 1;
        let candidate_a_slot = usize::from(swap_pairs);
        let candidate_b_slot = usize::from(!swap_pairs);
        let redis_a_slot = 2 + usize::from(swap_pairs);
        let redis_b_slot = 2 + usize::from(!swap_pairs);
        let mut elapsed = [Duration::ZERO; 4];
        let mut cpu_elapsed = [0_u64; 4];
        let mut groups_done = 0usize;
        let mut interleave_index = 0usize;
        while groups_done < groups {
            let block_groups = (groups - groups_done).min(interleave_groups);
            let order = ORDERS[(sample_index + interleave_index) % ORDERS.len()];
            for arm in order {
                let server_slot = match arm {
                    DualNullArm::CandidateA => candidate_a_slot,
                    DualNullArm::CandidateB => candidate_b_slot,
                    DualNullArm::RedisA => redis_a_slot,
                    DualNullArm::RedisB => redis_b_slot,
                };
                let cpu_before = servers[server_slot].cpu_ns();
                let block_elapsed = time_block(
                    &mut servers[server_slot],
                    &packets,
                    block_groups,
                    (groups_done % 2 == 1) ^ (sample_index % 2 == 1),
                );
                let cpu_after = servers[server_slot].cpu_ns();
                elapsed[arm.index()] += block_elapsed;
                cpu_elapsed[arm.index()] += cpu_after - cpu_before;
            }
            groups_done += block_groups;
            interleave_index += 1;
        }

        let candidate_a_cpu_ns = cpu_elapsed[DualNullArm::CandidateA.index()];
        let candidate_b_cpu_ns = cpu_elapsed[DualNullArm::CandidateB.index()];
        let redis_a_cpu_ns = cpu_elapsed[DualNullArm::RedisA.index()];
        let redis_b_cpu_ns = cpu_elapsed[DualNullArm::RedisB.index()];
        assert!(
            candidate_a_cpu_ns > 0
                && candidate_b_cpu_ns > 0
                && redis_a_cpu_ns > 0
                && redis_b_cpu_ns > 0,
            "every candidate and Redis A/A arm must accrue CPU time"
        );
        let candidate_a_ns = elapsed[DualNullArm::CandidateA.index()].as_nanos() as f64;
        let candidate_b_ns = elapsed[DualNullArm::CandidateB.index()].as_nanos() as f64;
        let redis_a_ns = elapsed[DualNullArm::RedisA.index()].as_nanos() as f64;
        let redis_b_ns = elapsed[DualNullArm::RedisB.index()].as_nanos() as f64;
        let candidate_center_ns = (candidate_a_ns * candidate_b_ns).sqrt();
        let redis_center_ns = (redis_a_ns * redis_b_ns).sqrt();
        let result = DualNullSample {
            candidate_a_ns,
            candidate_b_ns,
            redis_a_ns,
            redis_b_ns,
            candidate_null_ratio: candidate_a_ns / candidate_b_ns,
            redis_null_ratio: redis_a_ns / redis_b_ns,
            competitive_speedup: redis_center_ns / candidate_center_ns,
            candidate_a_cpu_ns,
            candidate_b_cpu_ns,
            redis_a_cpu_ns,
            redis_b_cpu_ns,
            candidate_a_cpu_util_pct: candidate_a_cpu_ns as f64 / candidate_a_ns * 100.0,
            candidate_b_cpu_util_pct: candidate_b_cpu_ns as f64 / candidate_b_ns * 100.0,
            redis_a_cpu_util_pct: redis_a_cpu_ns as f64 / redis_a_ns * 100.0,
            redis_b_cpu_util_pct: redis_b_cpu_ns as f64 / redis_b_ns * 100.0,
            cpu_candidate_null_ratio: candidate_a_cpu_ns as f64 / candidate_b_cpu_ns as f64,
            cpu_redis_null_ratio: redis_a_cpu_ns as f64 / redis_b_cpu_ns as f64,
            cpu_competitive_speedup: (redis_a_cpu_ns as f64 * redis_b_cpu_ns as f64).sqrt()
                / (candidate_a_cpu_ns as f64 * candidate_b_cpu_ns as f64).sqrt(),
        };
        println!(
            "SAMPLE_DUAL_NULL workload={} workload_shape={workload_shape} \
pipeline={pipeline} client_driver_threads={client_threads} sample={} \
first_order={:?} physical_pair_slots={} \
candidate_a_ns_per_op={:.3} candidate_b_ns_per_op={:.3} \
redis_a_ns_per_op={:.3} redis_b_ns_per_op={:.3} \
candidate_null_a_over_b={:.9} redis_null_a_over_b={:.9} \
candidate_over_redis={:.9} \
candidate_a_cpu_ns={} candidate_b_cpu_ns={} redis_a_cpu_ns={} redis_b_cpu_ns={} \
candidate_a_cpu_util_pct={:.3} candidate_b_cpu_util_pct={:.3} \
redis_a_cpu_util_pct={:.3} redis_b_cpu_util_pct={:.3} \
cpu_candidate_null_a_over_b={:.9} cpu_redis_null_a_over_b={:.9} \
cpu_candidate_over_redis={:.9}",
            workload.name(),
            sample_index + 1,
            ORDERS[sample_index % ORDERS.len()],
            if swap_pairs {
                "candidate_BA,redis_BA"
            } else {
                "candidate_AB,redis_AB"
            },
            result.candidate_a_ns / actual_ops as f64,
            result.candidate_b_ns / actual_ops as f64,
            result.redis_a_ns / actual_ops as f64,
            result.redis_b_ns / actual_ops as f64,
            result.candidate_null_ratio,
            result.redis_null_ratio,
            result.competitive_speedup,
            result.candidate_a_cpu_ns,
            result.candidate_b_cpu_ns,
            result.redis_a_cpu_ns,
            result.redis_b_cpu_ns,
            result.candidate_a_cpu_util_pct,
            result.candidate_b_cpu_util_pct,
            result.redis_a_cpu_util_pct,
            result.redis_b_cpu_util_pct,
            result.cpu_candidate_null_ratio,
            result.cpu_redis_null_ratio,
            result.cpu_competitive_speedup,
        );
        output.push(result);
    }
    if let Some(allowed_cpus) = host_wide_allowed_cpus {
        assert_host_wide_quiescence(
            allowed_cpus,
            &format!("after_dual_null_{}_{}", workload.name(), workload_shape),
        );
    }
    output
}

fn quantile(samples: &[f64], q: f64) -> f64 {
    assert!(!samples.is_empty(), "quantile requires samples");
    assert!((0.0..=1.0).contains(&q), "quantile must be in [0, 1]");
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = q * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        sorted[lower]
    } else {
        let fraction = position - lower as f64;
        sorted[lower] + (sorted[upper] - sorted[lower]) * fraction
    }
}

fn median(samples: &[f64]) -> f64 {
    quantile(samples, 0.5)
}

fn mean_cv_pct(samples: &[f64]) -> f64 {
    assert!(samples.len() >= 2, "CV requires at least two samples");
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / (samples.len() - 1) as f64;
    variance.sqrt() / mean * 100.0
}

fn bootstrap_median_ci(samples: &[f64]) -> (f64, f64) {
    assert!(
        samples.len() >= 8,
        "median CI requires at least eight paired samples"
    );
    const REPLICATES: usize = 20_000;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ samples.len() as u64;
    let mut resample = vec![0.0; samples.len()];
    let mut medians = Vec::with_capacity(REPLICATES);
    for _ in 0..REPLICATES {
        for value in &mut resample {
            // Deterministic xorshift64*: reproducible CI, no RNG dependency.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let draw = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            *value = samples[(draw as usize) % samples.len()];
        }
        medians.push(median(&resample));
    }
    (quantile(&medians, 0.025), quantile(&medians, 0.975))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Keep,
    Reject,
    Hold,
    Invalid,
}

fn adjudicate_ratios(
    metric: &str,
    ratio_name: &str,
    workload: Workload,
    pipeline: usize,
    client_threads: usize,
    null: &[f64],
    candidate: &[f64],
) -> Verdict {
    let null_median = median(null);
    let (null_ci_low, null_ci_high) = bootstrap_median_ci(null);
    let candidate_median = median(candidate);
    let (candidate_ci_low, candidate_ci_high) = bootstrap_median_ci(candidate);
    let null_radius = (null_ci_low - 1.0).abs().max((null_ci_high - 1.0).abs());
    let gate_low = 1.0 - 2.0 * null_radius;
    let gate_high = 1.0 + 2.0 * null_radius;
    let null_cv_pct = mean_cv_pct(null);
    let candidate_cv_pct = mean_cv_pct(candidate);
    let null_ci_brackets_one = null_ci_low <= 1.0 && null_ci_high >= 1.0;
    // A precise null CI can sit narrowly to one side of 1.0 under a small,
    // consistent arm-order bias. Requiring the CI itself to straddle 1.0 would
    // perversely reject more precise nulls. Bound that bias by the null median
    // instead; retain the CI and its straddle status as telemetry.
    let invalid = (null_median - 1.0).abs() > 0.02;
    let verdict = if invalid {
        Verdict::Invalid
    } else if candidate_ci_low > gate_high && candidate_median >= 1.01 {
        Verdict::Keep
    } else if candidate_ci_high < gate_low {
        Verdict::Reject
    } else {
        Verdict::Hold
    };
    println!(
        "MEDIAN_CI_GATE metric={metric} workload={} pipeline={pipeline} \
client_driver_threads={client_threads} verdict={verdict:?} \
null_median={null_median:.9} null_ci95=[{null_ci_low:.9},{null_ci_high:.9}] \
null_ci_brackets_one={null_ci_brackets_one} null_cv_pct={null_cv_pct:.6} \
margin2x=[{gate_low:.9},{gate_high:.9}] \
{ratio_name}_median={candidate_median:.9} \
candidate_ci95=[{candidate_ci_low:.9},{candidate_ci_high:.9}] \
candidate_cv_pct={candidate_cv_pct:.6}",
        workload.name()
    );
    assert!(
        !invalid,
        "INVALID A/A: metric={metric} workload={} pipeline={pipeline} \
client_driver_threads={client_threads} \
null median {null_median:.9} CI [{null_ci_low:.9},{null_ci_high:.9}] \
must remain within the 2% gross-bias guard; CI straddle is telemetry only",
        workload.name()
    );
    verdict
}

struct DualNullRatioSamples<'a> {
    candidate_null: &'a [f64],
    redis_null: &'a [f64],
    ratio: &'a [f64],
}

fn adjudicate_dual_null_ratios(
    metric: &str,
    ratio_name: &str,
    workload: Workload,
    pipeline: usize,
    client_threads: usize,
    samples: DualNullRatioSamples<'_>,
) -> Verdict {
    let DualNullRatioSamples {
        candidate_null,
        redis_null,
        ratio,
    } = samples;
    let candidate_null_median = median(candidate_null);
    let (candidate_null_ci_low, candidate_null_ci_high) = bootstrap_median_ci(candidate_null);
    let redis_null_median = median(redis_null);
    let (redis_null_ci_low, redis_null_ci_high) = bootstrap_median_ci(redis_null);
    let ratio_median = median(ratio);
    let (ratio_ci_low, ratio_ci_high) = bootstrap_median_ci(ratio);
    let candidate_null_radius = (candidate_null_ci_low - 1.0)
        .abs()
        .max((candidate_null_ci_high - 1.0).abs());
    let redis_null_radius = (redis_null_ci_low - 1.0)
        .abs()
        .max((redis_null_ci_high - 1.0).abs());
    let widest_null_radius = candidate_null_radius.max(redis_null_radius);
    let gate_low = 1.0 - 2.0 * widest_null_radius;
    let gate_high = 1.0 + 2.0 * widest_null_radius;
    let candidate_null_ci_brackets_one =
        candidate_null_ci_low <= 1.0 && candidate_null_ci_high >= 1.0;
    let redis_null_ci_brackets_one = redis_null_ci_low <= 1.0 && redis_null_ci_high >= 1.0;
    let invalid =
        (candidate_null_median - 1.0).abs() > 0.02 || (redis_null_median - 1.0).abs() > 0.02;
    let verdict = if invalid {
        Verdict::Invalid
    } else if ratio_ci_low > gate_high && ratio_median >= 1.01 {
        Verdict::Keep
    } else if ratio_ci_high < gate_low {
        Verdict::Reject
    } else {
        Verdict::Hold
    };
    println!(
        "MEDIAN_CI_DUAL_NULL_GATE metric={metric} workload={} pipeline={pipeline} \
client_driver_threads={client_threads} verdict={verdict:?} \
candidate_null_median={candidate_null_median:.9} \
candidate_null_ci95=[{candidate_null_ci_low:.9},{candidate_null_ci_high:.9}] \
candidate_null_ci_brackets_one={candidate_null_ci_brackets_one} \
candidate_null_cv_pct={:.6} \
redis_null_median={redis_null_median:.9} \
redis_null_ci95=[{redis_null_ci_low:.9},{redis_null_ci_high:.9}] \
redis_null_ci_brackets_one={redis_null_ci_brackets_one} \
redis_null_cv_pct={:.6} \
margin2x_widest_null=[{gate_low:.9},{gate_high:.9}] \
{ratio_name}_median={ratio_median:.9} \
ratio_ci95=[{ratio_ci_low:.9},{ratio_ci_high:.9}] \
ratio_cv_pct={:.6}",
        workload.name(),
        mean_cv_pct(candidate_null),
        mean_cv_pct(redis_null),
        mean_cv_pct(ratio)
    );
    assert!(
        !invalid,
        "INVALID A/A: metric={metric} workload={} pipeline={pipeline} \
client_driver_threads={client_threads}; candidate null median \
{candidate_null_median:.9} CI [{candidate_null_ci_low:.9},{candidate_null_ci_high:.9}] \
and Redis null median {redis_null_median:.9} \
CI [{redis_null_ci_low:.9},{redis_null_ci_high:.9}] must each remain within \
the 2% gross-bias guard; CI straddle is telemetry only",
        workload.name()
    );
    verdict
}

fn adjudicate(
    workload: Workload,
    pipeline: usize,
    client_threads: usize,
    samples: &[Sample],
) -> (Verdict, Verdict, Verdict, Verdict) {
    let io_uring_util = samples
        .iter()
        .map(|sample| sample.io_uring_cpu_util_pct)
        .collect::<Vec<_>>();
    let redis_util = samples
        .iter()
        .map(|sample| sample.redis_cpu_util_pct)
        .collect::<Vec<_>>();
    let io_uring_util_median = median(&io_uring_util);
    let redis_util_median = median(&redis_util);
    println!(
        "SERVER_SATURATION_GUARD workload={} pipeline={pipeline} \
client_driver_threads={client_threads} \
io_uring_cpu_util_median_pct={io_uring_util_median:.3} \
redis_cpu_util_median_pct={redis_util_median:.3} minimum_pct={MIN_SERVER_UTIL_PCT:.3}",
        workload.name()
    );
    assert!(
        io_uring_util_median >= MIN_SERVER_UTIL_PCT && redis_util_median >= MIN_SERVER_UTIL_PCT,
        "CLIENT-BOUND workload={} pipeline={pipeline} \
client_driver_threads={client_threads}: server utilization \
must reach {MIN_SERVER_UTIL_PCT:.1}% before wall throughput is admissible; \
io_uring={io_uring_util_median:.3}% redis={redis_util_median:.3}%",
        workload.name()
    );
    let wall_null = samples
        .iter()
        .map(|sample| sample.null_ratio)
        .collect::<Vec<_>>();
    let wall_candidate = samples
        .iter()
        .map(|sample| sample.self_speedup)
        .collect::<Vec<_>>();
    let wall_competitive = samples
        .iter()
        .map(|sample| sample.competitive_speedup)
        .collect::<Vec<_>>();
    let cpu_null = samples
        .iter()
        .map(|sample| sample.cpu_null_ratio)
        .collect::<Vec<_>>();
    let cpu_candidate = samples
        .iter()
        .map(|sample| sample.cpu_self_speedup)
        .collect::<Vec<_>>();
    let cpu_competitive = samples
        .iter()
        .map(|sample| sample.cpu_competitive_speedup)
        .collect::<Vec<_>>();
    (
        adjudicate_ratios(
            "wall_ns_per_op",
            "control_geomean_over_candidate",
            workload,
            pipeline,
            client_threads,
            &wall_null,
            &wall_candidate,
        ),
        adjudicate_ratios(
            "cpu_ns_per_fixed_work",
            "cpu_control_geomean_over_candidate",
            workload,
            pipeline,
            client_threads,
            &cpu_null,
            &cpu_candidate,
        ),
        adjudicate_ratios(
            "wall_ns_per_op",
            "candidate_over_redis",
            workload,
            pipeline,
            client_threads,
            &wall_null,
            &wall_competitive,
        ),
        adjudicate_ratios(
            "cpu_ns_per_fixed_work",
            "cpu_candidate_over_redis",
            workload,
            pipeline,
            client_threads,
            &cpu_null,
            &cpu_competitive,
        ),
    )
}

fn adjudicate_dual_null(
    workload: Workload,
    pipeline: usize,
    client_threads: usize,
    samples: &[DualNullSample],
) -> (Verdict, Verdict) {
    let candidate_a_util = samples
        .iter()
        .map(|sample| sample.candidate_a_cpu_util_pct)
        .collect::<Vec<_>>();
    let candidate_b_util = samples
        .iter()
        .map(|sample| sample.candidate_b_cpu_util_pct)
        .collect::<Vec<_>>();
    let redis_a_util = samples
        .iter()
        .map(|sample| sample.redis_a_cpu_util_pct)
        .collect::<Vec<_>>();
    let redis_b_util = samples
        .iter()
        .map(|sample| sample.redis_b_cpu_util_pct)
        .collect::<Vec<_>>();
    let candidate_a_util_median = median(&candidate_a_util);
    let candidate_b_util_median = median(&candidate_b_util);
    let redis_a_util_median = median(&redis_a_util);
    let redis_b_util_median = median(&redis_b_util);
    println!(
        "SERVER_SATURATION_DUAL_NULL_GUARD workload={} pipeline={pipeline} \
client_driver_threads={client_threads} \
candidate_a_cpu_util_median_pct={candidate_a_util_median:.3} \
candidate_b_cpu_util_median_pct={candidate_b_util_median:.3} \
redis_a_cpu_util_median_pct={redis_a_util_median:.3} \
redis_b_cpu_util_median_pct={redis_b_util_median:.3} \
minimum_pct={MIN_SERVER_UTIL_PCT:.3}",
        workload.name()
    );
    assert!(
        [
            candidate_a_util_median,
            candidate_b_util_median,
            redis_a_util_median,
            redis_b_util_median,
        ]
        .into_iter()
        .all(|utilization| utilization >= MIN_SERVER_UTIL_PCT),
        "CLIENT-BOUND workload={} pipeline={pipeline} \
client_driver_threads={client_threads}: every candidate and Redis A/A arm must reach \
{MIN_SERVER_UTIL_PCT:.1}% server utilization; candidate_a={candidate_a_util_median:.3}% \
candidate_b={candidate_b_util_median:.3}% redis_a={redis_a_util_median:.3}% \
redis_b={redis_b_util_median:.3}%",
        workload.name()
    );
    let candidate_wall_null = samples
        .iter()
        .map(|sample| sample.candidate_null_ratio)
        .collect::<Vec<_>>();
    let redis_wall_null = samples
        .iter()
        .map(|sample| sample.redis_null_ratio)
        .collect::<Vec<_>>();
    let wall_competitive = samples
        .iter()
        .map(|sample| sample.competitive_speedup)
        .collect::<Vec<_>>();
    let candidate_cpu_null = samples
        .iter()
        .map(|sample| sample.cpu_candidate_null_ratio)
        .collect::<Vec<_>>();
    let redis_cpu_null = samples
        .iter()
        .map(|sample| sample.cpu_redis_null_ratio)
        .collect::<Vec<_>>();
    let cpu_competitive = samples
        .iter()
        .map(|sample| sample.cpu_competitive_speedup)
        .collect::<Vec<_>>();
    (
        adjudicate_dual_null_ratios(
            "wall_ns_per_op",
            "candidate_over_redis",
            workload,
            pipeline,
            client_threads,
            DualNullRatioSamples {
                candidate_null: &candidate_wall_null,
                redis_null: &redis_wall_null,
                ratio: &wall_competitive,
            },
        ),
        adjudicate_dual_null_ratios(
            "cpu_ns_per_fixed_work",
            "cpu_candidate_over_redis",
            workload,
            pipeline,
            client_threads,
            DualNullRatioSamples {
                candidate_null: &candidate_cpu_null,
                redis_null: &redis_cpu_null,
                ratio: &cpu_competitive,
            },
        ),
    )
}

fn aggregate_libc_self_pct(report: &str) -> f64 {
    report
        .lines()
        .filter(|line| line.contains("libc.so"))
        .filter_map(|line| {
            line.split_whitespace()
                .next()?
                .trim_end_matches('%')
                .parse::<f64>()
                .ok()
        })
        .sum()
}

fn dso_offset(line: &str, marker: &str) -> Option<u64> {
    let marker_start = line.find(marker)?;
    let hex_start = marker_start + marker.len();
    let hex = line[hex_start..]
        .split(|character: char| !character.is_ascii_hexdigit())
        .next()?;
    (!hex.is_empty()).then(|| u64::from_str_radix(hex, 16).expect("perf DSO offset is hexadecimal"))
}

fn sampled_symbol(line: &str) -> Option<&str> {
    line.split_once(" (")?.0.split_whitespace().nth(1)
}

#[derive(Debug, Default)]
struct LibcLeafCallers {
    samples: usize,
    leaf_offsets: HashSet<u64>,
    caller_counts: HashMap<u64, usize>,
}

#[derive(Debug)]
struct DominantLibcLeafCallers {
    identity: String,
    samples: usize,
    total_samples: usize,
    leaf_offsets: Vec<u64>,
    caller_counts: Vec<(u64, usize)>,
}

fn dominant_libc_leaf_callers(script: &str, executable: &Path) -> Option<DominantLibcLeafCallers> {
    let executable_name = executable
        .file_name()
        .and_then(|name| name.to_str())
        .expect("profiled executable name must be UTF-8");
    let executable_marker = format!("{executable_name}+0x");
    let mut groups = HashMap::<String, LibcLeafCallers>::new();
    let mut total_samples = 0usize;
    let mut expecting_leaf = false;
    let mut active_group = None::<String>;
    let mut sample_callers = HashSet::<u64>::new();

    for line in script.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty()
            && trimmed.split_whitespace().count() == 1
            && trimmed.parse::<u64>().is_ok()
        {
            total_samples += 1;
            expecting_leaf = true;
            active_group = None;
            sample_callers.clear();
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if expecting_leaf {
            expecting_leaf = false;
            let Some(offset) = dso_offset(line, "libc.so.6+0x") else {
                continue;
            };
            let symbol = sampled_symbol(line).unwrap_or("[unknown]");
            let identity = if ["memcmp", "memcpy", "memmove", "memset"]
                .iter()
                .any(|family| symbol.contains(family))
            {
                format!("symbol:{symbol}")
            } else if symbol == "[unknown]" || symbol.starts_with("0x") {
                format!("unresolved_libc_page:0x{:x}", offset & !0xfff)
            } else {
                continue;
            };
            let group = groups.entry(identity.clone()).or_default();
            group.samples += 1;
            group.leaf_offsets.insert(offset);
            active_group = Some(identity);
            continue;
        }
        let Some(identity) = active_group.as_ref() else {
            continue;
        };
        let Some(offset) = dso_offset(line, &executable_marker) else {
            continue;
        };
        if sample_callers.insert(offset) {
            *groups
                .get_mut(identity)
                .expect("active libc group exists")
                .caller_counts
                .entry(offset)
                .or_default() += 1;
        }
    }

    let (identity, group) = groups.into_iter().max_by_key(|(_, group)| group.samples)?;
    let self_pct = 100.0 * group.samples as f64 / total_samples as f64;
    if self_pct < HOT_LIBC_LEAF_MIN_SELF_PCT {
        return None;
    }
    let mut leaf_offsets = group.leaf_offsets.into_iter().collect::<Vec<_>>();
    leaf_offsets.sort_unstable();
    let mut caller_counts = group.caller_counts.into_iter().collect::<Vec<_>>();
    caller_counts.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    caller_counts.truncate(48);
    Some(DominantLibcLeafCallers {
        identity,
        samples: group.samples,
        total_samples,
        leaf_offsets,
        caller_counts,
    })
}

fn attribute_hot_libc_leaf(candidate: &Server, data: &Path, self_report: &str, context: &str) {
    let aggregate_libc_self_pct = aggregate_libc_self_pct(self_report);
    if aggregate_libc_self_pct < HOT_LIBC_LEAF_MIN_SELF_PCT {
        println!(
            "PROFILE_LIBC_LEAF_ATTRIBUTION {context} \
status=no_dominant_memory_leaf aggregate_libc_self_pct={aggregate_libc_self_pct:.4} \
minimum_self_pct={HOT_LIBC_LEAF_MIN_SELF_PCT:.3}"
        );
        return;
    }

    let executable = fs::read_link(format!("/proc/{}/exe", candidate.pid()))
        .expect("resolve profiled executable");
    let script = Command::new("perf")
        .env("LC_ALL", "C")
        .args(["script", "-F", "period,ip,sym,dso,dsoff", "-i"])
        .arg(data)
        .output()
        .expect("run perf script for dominant libc leaf");
    assert!(
        script.status.success(),
        "perf script failed for libc caller attribution: {}",
        String::from_utf8_lossy(&script.stderr)
    );
    let script = String::from_utf8(script.stdout).expect("perf script output is UTF-8");
    let Some(callers) = dominant_libc_leaf_callers(&script, &executable) else {
        println!(
            "PROFILE_LIBC_LEAF_ATTRIBUTION {context} \
status=no_single_dominant_libc_leaf aggregate_libc_self_pct={aggregate_libc_self_pct:.4} \
minimum_self_pct={HOT_LIBC_LEAF_MIN_SELF_PCT:.3}"
        );
        return;
    };
    assert!(
        !callers.caller_counts.is_empty(),
        "DWARF call chains for {} did not reach the profiled executable",
        callers.identity
    );

    let caller_offsets = callers
        .caller_counts
        .iter()
        .map(|(offset, _)| format!("0x{offset:x}"))
        .collect::<Vec<_>>();
    let mut addr2line = Command::new("addr2line");
    addr2line
        .args(["-a", "-f", "-C", "-i", "-p", "-e"])
        .arg(&executable)
        .args(&caller_offsets);
    let addr2line = addr2line
        .output()
        .expect("run addr2line for dominant libc leaf callers");
    assert!(
        addr2line.status.success(),
        "addr2line failed for {}: {}",
        callers.identity,
        String::from_utf8_lossy(&addr2line.stderr)
    );
    let inline_callers = String::from_utf8(addr2line.stdout)
        .expect("addr2line output is UTF-8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(256)
        .map(str::trim)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let leaf_offsets = callers
        .leaf_offsets
        .iter()
        .map(|offset| format!("0x{offset:x}"))
        .collect::<Vec<_>>();
    let weighted_callers = callers
        .caller_counts
        .iter()
        .map(|(offset, count)| {
            (
                format!("0x{offset:x}"),
                *count,
                100.0 * *count as f64 / callers.samples as f64,
            )
        })
        .collect::<Vec<_>>();
    let self_pct = 100.0 * callers.samples as f64 / callers.total_samples as f64;

    println!(
        "PROFILE_LIBC_LEAF_ATTRIBUTION {context} leaf={} self_pct={self_pct:.4} \
leaf_samples={} total_samples={} aggregate_libc_self_pct={aggregate_libc_self_pct:.4} \
unwind=dwarf_8192 addr2line_inlines=true executing_elf_sha256={} \
leaf_offsets={leaf_offsets:?} weighted_caller_offsets={weighted_callers:?} \
inline_callers={inline_callers:?}",
        callers.identity,
        callers.samples,
        callers.total_samples,
        candidate.executing_elf_sha256()
    );
}

fn profile_io_uring_path(
    candidate: &mut Server,
    root: &Path,
    profile_seconds: u64,
    workload: Workload,
    pipeline: usize,
    client_threads: usize,
) {
    let data = root.join(format!(
        "io_uring_profile_{}_p{pipeline}_ct{client_threads}.data",
        workload.name()
    ));
    assert!(!data.exists(), "refusing to overwrite {}", data.display());
    let mut perf = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "record",
            "-q",
            "-F",
            "997",
            "-e",
            "cycles",
            "-g",
            "--call-graph",
            "dwarf,8192",
            "-p",
            &candidate.pid().to_string(),
            "-o",
        ])
        .arg(&data)
        .args(["--", "sleep", &profile_seconds.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn perf record");
    thread::sleep(Duration::from_millis(500));
    if let Some(status) = perf.try_wait().expect("poll perf record") {
        let mut stderr = String::new();
        perf.stderr
            .take()
            .expect("capture early perf stderr")
            .read_to_string(&mut stderr)
            .expect("read early perf stderr");
        panic!("perf record exited before profile workload: status={status} stderr={stderr}");
    }

    let packets = Arc::new(DriverPackets::shared(workload, pipeline));
    while perf
        .try_wait()
        .expect("poll perf record workload")
        .is_none()
    {
        time_block(candidate, &packets, 32, false);
    }
    let perf_output = perf.wait_with_output().expect("wait for perf record");
    assert!(
        perf_output.status.success(),
        "perf record failed: {}",
        String::from_utf8_lossy(&perf_output.stderr)
    );

    let report = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--percent-limit",
            "0",
            "--call-graph",
            "none",
            "--sort",
            "overhead,symbol,dso",
            "-i",
        ])
        .arg(&data)
        .output()
        .expect("run perf report");
    assert!(
        report.status.success(),
        "perf report failed: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report = String::from_utf8(report.stdout).expect("perf report is UTF-8");
    let lost = report
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .expect("perf report states lost-sample count");
    assert!(
        lost.trim_end().ends_with(" 0"),
        "profile lost samples: {lost}"
    );
    let top_self_rows = report
        .lines()
        .filter(|line| {
            line.split_whitespace()
                .next()
                .and_then(|raw| raw.trim_end_matches('%').parse::<f64>().ok())
                .is_some()
        })
        .take(32)
        .map(str::trim)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    println!(
        "PROFILE_TOP_SELF workload={} pipeline={pipeline} \
client_driver_threads={client_threads} rows={top_self_rows:?}",
        workload.name()
    );

    let owned_targets = ["BatchWriter>::submit_owned", "BatchWriter>::drain_owned"];
    let surface_targets = [
        owned_targets[0],
        owned_targets[1],
        "frankenredis::submit_uring_batch",
        "frankenredis::drain_uring_completions",
        "io_uring::submit::Submitter>::submit_and_wait",
        "io_uring_enter",
    ];
    let mut matched = Vec::new();
    let mut owned_self_pct = 0.0_f64;
    let mut surface_self_pct = 0.0_f64;
    for line in report.lines() {
        if surface_targets.iter().any(|target| line.contains(target))
            && let Some(raw_pct) = line.split_whitespace().next()
            && let Ok(pct) = raw_pct.trim_end_matches('%').parse::<f64>()
        {
            surface_self_pct += pct;
            if owned_targets.iter().any(|target| line.contains(target)) {
                owned_self_pct += pct;
            }
            matched.push(line.trim().to_owned());
        }
    }
    let command_targets = workload.profile_targets();
    // The profile gate follows the function under test. Pure output workloads
    // target the owned io_uring path; command-shape workloads gate below on
    // their named command surface while retaining async output as provenance.
    if command_targets.is_empty() {
        assert!(
            owned_self_pct > 0.0,
            "profile did not attribute non-zero self-time to owned submit/CQ drain"
        );
    }
    assert!(
        surface_self_pct < 100.0,
        "invalid aggregate io_uring self-time: {surface_self_pct}%"
    );
    let amdahl_ceiling = 1.0 / (1.0 - surface_self_pct / 100.0);
    println!(
        "PROFILE_REACHABILITY target=async_owned_io_uring_output \
workload={} pipeline={pipeline} client_driver_threads={client_threads} \
owned_self_pct={owned_self_pct:.4} surface_self_pct={surface_self_pct:.4} \
amdahl_elimination_ceiling={amdahl_ceiling:.6}x lost_samples=0 rows={matched:?}",
        workload.name()
    );

    if !command_targets.is_empty() {
        let mut command_rows = Vec::new();
        let mut command_self_pct = 0.0_f64;
        for line in report.lines() {
            if command_targets.iter().any(|target| line.contains(target))
                && let Some(raw_pct) = line.split_whitespace().next()
                && let Ok(pct) = raw_pct.trim_end_matches('%').parse::<f64>()
            {
                command_self_pct += pct;
                command_rows.push(line.trim().to_owned());
            }
        }
        assert!(
            command_self_pct > 0.0,
            "profile did not attribute non-zero self-time to workload={} targets={command_targets:?}",
            workload.name()
        );
        assert!(
            command_self_pct < 100.0,
            "invalid aggregate command self-time: {command_self_pct}%"
        );
        let command_amdahl_ceiling = 1.0 / (1.0 - command_self_pct / 100.0);
        println!(
            "PROFILE_COMMAND_SURFACE workload={} pipeline={pipeline} \
client_driver_threads={client_threads} targets={command_targets:?} \
self_pct={command_self_pct:.4} \
amdahl_elimination_ceiling={command_amdahl_ceiling:.6}x \
lost_samples=0 rows={command_rows:?}",
            workload.name()
        );
    }
    attribute_hot_libc_leaf(
        candidate,
        &data,
        &report,
        &format!(
            "workload={} pipeline={pipeline} client_driver_threads={client_threads}",
            workload.name()
        ),
    );
}

const SHARDED_PROFILE_TARGETS: [&str; 6] = [
    "frankenredis::dispatch_sharded_set_get_frames",
    "frankenredis::drain_sharded_set_get_completions",
    "<fr_runtime::Runtime>::execute_plain_set_borrowed",
    "<fr_runtime::Runtime>::execute_plain_get_borrowed",
    "<fr_store::Store>::set_plain_borrowed",
    "<fr_store::Store>::get_string_bytes",
];

fn matching_profile_self_time(report: &str, targets: &[&str]) -> (f64, Vec<String>) {
    let mut rows = Vec::new();
    let mut self_pct = 0.0_f64;
    for line in report.lines() {
        if targets.iter().any(|target| line.contains(target))
            && let Some(raw_pct) = line.split_whitespace().next()
            && let Ok(pct) = raw_pct.trim_end_matches('%').parse::<f64>()
        {
            self_pct += pct;
            rows.push(line.trim().to_owned());
        }
    }
    (self_pct, rows)
}

fn profile_sharded_set_get_path(
    candidate: &mut Server,
    root: &Path,
    profile_seconds: u64,
    workload: Workload,
    pipeline: usize,
    server_workers: usize,
    packets: &Arc<DriverPackets>,
) {
    candidate
        .clients
        .as_ref()
        .expect("benchmark clients initialized")
        .prepare(packets);
    let data = root.join(format!(
        "sharded_set_get_profile_{}_p{pipeline}_sw{server_workers}.data",
        workload.name()
    ));
    assert!(!data.exists(), "refusing to overwrite {}", data.display());
    let mut perf = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "record",
            "-q",
            "-F",
            "997",
            "-e",
            "cycles",
            "-g",
            "--call-graph",
            "dwarf,8192",
            "-p",
            &candidate.pid().to_string(),
            "-o",
        ])
        .arg(&data)
        .args(["--", "sleep", &profile_seconds.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sharded SET/GET perf record");
    thread::sleep(Duration::from_millis(500));
    if let Some(status) = perf.try_wait().expect("poll sharded perf record") {
        let mut stderr = String::new();
        perf.stderr
            .take()
            .expect("capture early sharded perf stderr")
            .read_to_string(&mut stderr)
            .expect("read early sharded perf stderr");
        panic!(
            "sharded perf record exited before profile workload: status={status} stderr={stderr}"
        );
    }
    while perf
        .try_wait()
        .expect("poll sharded perf record workload")
        .is_none()
    {
        time_block(candidate, packets, 32, false);
    }
    let perf_output = perf
        .wait_with_output()
        .expect("wait for sharded perf record");
    assert!(
        perf_output.status.success(),
        "sharded perf record failed: {}",
        String::from_utf8_lossy(&perf_output.stderr)
    );

    let report = Command::new("perf")
        .env("LC_ALL", "C")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--percent-limit",
            "0",
            "--call-graph",
            "none",
            "--sort",
            "overhead,symbol,dso",
            "-i",
        ])
        .arg(&data)
        .output()
        .expect("run sharded perf report");
    assert!(
        report.status.success(),
        "sharded perf report failed: {}",
        String::from_utf8_lossy(&report.stderr)
    );
    let report = String::from_utf8(report.stdout).expect("sharded perf report is UTF-8");
    let lost = report
        .lines()
        .find(|line| line.contains("Total Lost Samples:"))
        .expect("sharded perf report states lost-sample count");
    assert!(
        lost.trim_end().ends_with(" 0"),
        "sharded profile lost samples: {lost}"
    );
    let top_self_rows = report
        .lines()
        .filter(|line| {
            line.split_whitespace()
                .next()
                .and_then(|raw| raw.trim_end_matches('%').parse::<f64>().ok())
                .is_some()
        })
        .take(32)
        .map(str::trim)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let (target_self_pct, target_rows) =
        matching_profile_self_time(&report, &SHARDED_PROFILE_TARGETS);
    assert!(
        target_self_pct > 0.0,
        "profile did not attribute non-zero self-time to the sharded command bus"
    );
    assert!(
        target_self_pct < 100.0,
        "invalid aggregate sharded command-bus self-time: {target_self_pct}%"
    );
    let amdahl_ceiling = 1.0 / (1.0 - target_self_pct / 100.0);
    println!(
        "PROFILE_SHARDED_COMMAND_SURFACE workload={} pipeline={pipeline} \
server_command_execution_threads={server_workers} target_self_pct={target_self_pct:.4} \
amdahl_elimination_ceiling={amdahl_ceiling:.6}x lost_samples=0 \
targets={SHARDED_PROFILE_TARGETS:?} rows={target_rows:?} top_self_rows={top_self_rows:?}",
        workload.name()
    );
    attribute_hot_libc_leaf(
        candidate,
        &data,
        &report,
        &format!(
            "workload={} pipeline={pipeline} server_command_execution_threads={server_workers}",
            workload.name()
        ),
    );
}

fn parse_cpu_list(text: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for item in text.trim().split(',') {
        if let Some((start, end)) = item.split_once('-') {
            let start = start.parse::<usize>().expect("parse CPU range start");
            let end = end.parse::<usize>().expect("parse CPU range end");
            cpus.extend(start..=end);
        } else if !item.is_empty() {
            cpus.push(item.parse::<usize>().expect("parse CPU number"));
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    cpus
}

fn allowed_cpus_for_pid(pid: u32) -> Vec<usize> {
    let status =
        fs::read_to_string(format!("/proc/{pid}/status")).expect("read process CPU allowance");
    let allowed = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(str::trim)
        .expect("Cpus_allowed_list is present");
    parse_cpu_list(allowed)
}

fn allowed_cpus() -> Vec<usize> {
    allowed_cpus_for_pid(std::process::id())
}

fn machine_topology() -> (usize, usize) {
    let online = fs::read_to_string("/sys/devices/system/cpu/online")
        .map(|text| parse_cpu_list(&text))
        .unwrap_or_else(|_| {
            (0..thread::available_parallelism().expect("detect CPUs").get()).collect()
        });
    let mut physical_cores = HashSet::new();
    for cpu in &online {
        let package = fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/physical_package_id"
        ))
        .expect("read physical package id");
        let core = fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_id"))
            .expect("read physical core id");
        physical_cores.insert((package.trim().to_owned(), core.trim().to_owned()));
    }
    (physical_cores.len(), online.len())
}

fn machine_ram_kib() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .expect("read machine memory provenance")
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == "MemTotal:").then(|| {
                fields
                    .next()
                    .expect("MemTotal value is present")
                    .parse::<u64>()
                    .expect("MemTotal is an integer")
            })
        })
        .expect("MemTotal is present")
}

fn machine_numa_nodes() -> usize {
    fs::read_to_string("/sys/devices/system/node/online")
        .map(|text| parse_cpu_list(&text).len())
        .unwrap_or(1)
}

#[derive(Debug, PartialEq, Eq)]
struct CpuFrequencyPolicy {
    scaling_drivers: Vec<String>,
    scaling_governors: Vec<String>,
    energy_performance_preferences: Vec<String>,
    boost: String,
    amd_pstate_status: String,
    intel_pstate_no_turbo: String,
}

fn read_policy_value(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn per_cpu_policy_values(cpus: &[usize], field: &str) -> Vec<String> {
    let mut values = cpus
        .iter()
        .map(|cpu| read_policy_value(format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/{field}")))
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    if values.is_empty() {
        values.push("unavailable".to_owned());
    }
    values
}

fn cpu_frequency_policy(cpus: &[usize]) -> CpuFrequencyPolicy {
    CpuFrequencyPolicy {
        scaling_drivers: per_cpu_policy_values(cpus, "scaling_driver"),
        scaling_governors: per_cpu_policy_values(cpus, "scaling_governor"),
        energy_performance_preferences: per_cpu_policy_values(
            cpus,
            "energy_performance_preference",
        ),
        boost: read_policy_value("/sys/devices/system/cpu/cpufreq/boost"),
        amd_pstate_status: read_policy_value("/sys/devices/system/cpu/amd_pstate/status"),
        intel_pstate_no_turbo: read_policy_value("/sys/devices/system/cpu/intel_pstate/no_turbo"),
    }
}

fn print_cpu_frequency_policy(phase: &str, cpus: &[usize], policy: &CpuFrequencyPolicy) {
    let cpufreq_control_visible = policy
        .scaling_drivers
        .iter()
        .any(|value| value != "unavailable")
        && policy
            .scaling_governors
            .iter()
            .any(|value| value != "unavailable");
    println!(
        "CPU_FREQUENCY_POLICY phase={phase} cpus_checked={cpus:?} \
scaling_drivers={:?} scaling_governors={:?} \
energy_performance_preferences={:?} boost={} amd_pstate_status={} \
intel_pstate_no_turbo={} cpufreq_control_visible={cpufreq_control_visible}",
        policy.scaling_drivers,
        policy.scaling_governors,
        policy.energy_performance_preferences,
        policy.boost,
        policy.amd_pstate_status,
        policy.intel_pstate_no_turbo
    );
}

#[cfg(target_arch = "x86_64")]
fn runtime_isa_features() -> String {
    format!(
        "avx2={},fma={},bmi2={},vaes={},avx512f={}",
        std::arch::is_x86_feature_detected!("avx2"),
        std::arch::is_x86_feature_detected!("fma"),
        std::arch::is_x86_feature_detected!("bmi2"),
        std::arch::is_x86_feature_detected!("vaes"),
        std::arch::is_x86_feature_detected!("avx512f")
    )
}

#[cfg(target_arch = "aarch64")]
fn runtime_isa_features() -> String {
    format!(
        "neon={},aes={}",
        std::arch::is_aarch64_feature_detected!("neon"),
        std::arch::is_aarch64_feature_detected!("aes")
    )
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn runtime_isa_features() -> String {
    "runtime-detection-unavailable".to_owned()
}

fn sibling_group(cpu: usize) -> Vec<usize> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    fs::read_to_string(path)
        .map(|text| parse_cpu_list(&text))
        .unwrap_or_else(|_| vec![cpu])
}

fn read_core_ticks() -> HashMap<usize, (u64, u64)> {
    let stat = fs::read_to_string("/proc/stat").expect("read per-core CPU counters");
    stat.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let label = fields.next()?;
            let cpu = label.strip_prefix("cpu")?.parse::<usize>().ok()?;
            let ticks = fields
                .take(8)
                .map(|value| value.parse::<u64>().expect("parse /proc/stat CPU tick"))
                .collect::<Vec<_>>();
            assert!(
                ticks.len() >= 4,
                "per-core /proc/stat row has fewer than four counters: {line:?}"
            );
            let idle = ticks[3] + ticks.get(4).copied().unwrap_or(0);
            Some((cpu, (ticks.iter().sum(), idle)))
        })
        .collect()
}

fn observed_core_loads() -> HashMap<usize, f64> {
    let before = read_core_ticks();
    thread::sleep(Duration::from_millis(500));
    let after = read_core_ticks();
    after
        .into_iter()
        .filter_map(|(cpu, (total_after, idle_after))| {
            let (total_before, idle_before) = before.get(&cpu).copied()?;
            let total = total_after.saturating_sub(total_before);
            let idle = idle_after.saturating_sub(idle_before).min(total);
            (total != 0).then_some((cpu, 100.0 * (total - idle) as f64 / total as f64))
        })
        .collect()
}

fn host_wide_quiescence_violations(
    allowed_cpus: &[usize],
    loads: &HashMap<usize, f64>,
) -> Vec<String> {
    allowed_cpus
        .iter()
        .filter_map(|cpu| match loads.get(cpu) {
            Some(load) if *load <= HOST_WIDE_MAX_BUSY_PCT => None,
            Some(load) => Some(format!("cpu{cpu}={load:.1}%")),
            None => Some(format!("cpu{cpu}=missing")),
        })
        .collect()
}

fn assert_host_wide_quiescence(allowed_cpus: &[usize], phase: &str) {
    assert!(
        !allowed_cpus.is_empty(),
        "host-wide benchmark cpuset cannot be empty"
    );
    for attempt in 1..=QUIET_CORE_PREFLIGHT_ATTEMPTS {
        let loads = observed_core_loads();
        let violations = host_wide_quiescence_violations(allowed_cpus, &loads);
        if violations.is_empty() {
            let maximum_busy_pct = allowed_cpus
                .iter()
                .filter_map(|cpu| loads.get(cpu))
                .copied()
                .fold(0.0_f64, f64::max);
            println!(
                "HOST_WIDE_QUIESCENCE phase={phase} attempts={attempt} \
allowed_cpu_count={} sampled_cpu_count={} maximum_busy_pct={maximum_busy_pct:.3} \
limit_pct={HOST_WIDE_MAX_BUSY_PCT:.3} busy_cpu_count_above_limit=0 \
verdict=clear loads={loads:?}",
                allowed_cpus.len(),
                loads.len()
            );
            return;
        }
        if attempt == QUIET_CORE_PREFLIGHT_ATTEMPTS {
            panic!(
                "host-wide benchmark lost exclusivity during {phase} after {attempt} attempts; \
CPUs above {HOST_WIDE_MAX_BUSY_PCT:.1}% busy or missing: {}",
                violations.join(",")
            );
        }
        println!(
            "HOST_WIDE_QUIESCENCE_RETRY phase={phase} \
attempt={attempt}/{QUIET_CORE_PREFLIGHT_ATTEMPTS} \
allowed_cpu_count={} sampled_cpu_count={} \
violations={violations:?} loads={loads:?}",
            allowed_cpus.len(),
            loads.len()
        );
    }
    unreachable!("host-wide quiescence loop returns or panics");
}

fn current_process_cpu() -> usize {
    let stat = fs::read_to_string("/proc/self/stat").expect("read benchmark process stat");
    let after_comm = stat
        .rsplit_once(')')
        .map(|(_, suffix)| suffix)
        .expect("/proc/self/stat contains a parenthesized command name");
    after_comm
        .split_whitespace()
        .nth(36)
        .expect("/proc/self/stat contains processor field")
        .parse::<usize>()
        .expect("parse current processor")
}

fn choose_client_cpu_order(max_client_threads: usize) -> (Vec<usize>, usize, Vec<usize>) {
    let allowed = allowed_cpus();
    let allowed_set = allowed.iter().copied().collect::<HashSet<_>>();
    for attempt in 1..=QUIET_CORE_PREFLIGHT_ATTEMPTS {
        let observer_before = current_process_cpu();
        let loads = observed_core_loads();
        let observer_after = current_process_cpu();
        let observer_cpus = [observer_before, observer_after]
            .into_iter()
            .collect::<HashSet<_>>();
        let quiet = allowed
            .iter()
            .copied()
            .filter(|cpu| {
                sibling_group(*cpu).iter().all(|sibling| {
                    observer_cpus.contains(sibling)
                        || loads.get(sibling).copied().unwrap_or(0.0) < QUIET_CORE_MAX_PCT
                })
            })
            .collect::<Vec<_>>();
        let quiet_set = quiet.iter().copied().collect::<HashSet<_>>();
        let mut grouped = HashSet::new();
        let mut physical_groups = Vec::new();
        for cpu in &quiet {
            if grouped.contains(cpu) {
                continue;
            }
            let siblings = sibling_group(*cpu)
                .into_iter()
                .filter(|sibling| allowed_set.contains(sibling) && quiet_set.contains(sibling))
                .collect::<Vec<_>>();
            if !siblings.is_empty() {
                grouped.extend(siblings.iter().copied());
                physical_groups.push(siblings);
            }
        }
        physical_groups.sort_by_key(|group| group[0]);
        let Some(server_group) = physical_groups.pop() else {
            if attempt == QUIET_CORE_PREFLIGHT_ATTEMPTS {
                panic!(
                    "worker did not expose a quiet physical server core after {attempt} attempts: \
allowed={allowed:?} loads={loads:?} quiet={quiet:?}"
                );
            }
            println!(
                "CPU_PREFLIGHT_RETRY attempt={attempt}/{QUIET_CORE_PREFLIGHT_ATTEMPTS} \
allowed={allowed:?} loads={loads:?} quiet={quiet:?}"
            );
            continue;
        };
        let max_siblings = physical_groups.iter().map(Vec::len).max().unwrap_or(0);
        let mut client_cpu_order = Vec::new();
        for sibling_index in 0..max_siblings {
            for group in &physical_groups {
                if let Some(cpu) = group.get(sibling_index) {
                    client_cpu_order.push(*cpu);
                }
            }
        }
        let client_capacity_limit = allowed.len().saturating_sub(server_group.len());
        let desired_client_cpus = max_client_threads.min(client_capacity_limit);
        let minimum_client_cpus = if max_client_threads >= client_capacity_limit {
            desired_client_cpus.saturating_mul(9).div_ceil(10)
        } else {
            desired_client_cpus
        };
        if client_cpu_order.len() < minimum_client_cpus {
            if attempt == QUIET_CORE_PREFLIGHT_ATTEMPTS {
                panic!(
                    "worker did not expose at least {minimum_client_cpus} quiet logical client \
CPUs (desired {desired_client_cpus}) plus a \
disjoint physical server core after {attempt} attempts: allowed={allowed:?} loads={loads:?} \
quiet={quiet:?} client_cpu_order={client_cpu_order:?} server_group={server_group:?}"
                );
            }
            println!(
                "CPU_PREFLIGHT_RETRY attempt={attempt}/{QUIET_CORE_PREFLIGHT_ATTEMPTS} \
allowed={allowed:?} loads={loads:?} quiet={quiet:?} \
client_cpu_order={client_cpu_order:?} server_group={server_group:?} \
observer_cpus={observer_cpus:?} desired_client_cpus={desired_client_cpus} \
minimum_client_cpus={minimum_client_cpus}"
            );
            continue;
        }
        let server_core = server_group[0];
        println!(
            "CPU_PREFLIGHT attempts={attempt} max_client_threads={max_client_threads} \
client_cpu_order={client_cpu_order:?} server={server_core} \
server_siblings={server_group:?} process_cpuset_cap={allowed:?} \
observer_cpus={observer_cpus:?} client_affinity_capacity={} \
desired_client_cpus={desired_client_cpus} minimum_client_cpus={minimum_client_cpus} \
loads={loads:?}",
            client_cpu_order.len()
        );
        return (client_cpu_order, server_core, allowed);
    }
    unreachable!("quiet-core preflight loop returns or panics");
}

fn pin_client_process(client_cpu_order: &[usize], client_threads: usize) -> Vec<usize> {
    let affinity_cpu_count = client_threads.min(client_cpu_order.len());
    assert!(
        affinity_cpu_count > 0,
        "client affinity requires at least one logical CPU"
    );
    let client_affinity = client_cpu_order[..affinity_cpu_count].to_vec();
    let client_mask = client_affinity
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let pin = Command::new("taskset")
        .args(["-apc", &client_mask, &std::process::id().to_string()])
        .output()
        .expect("pin benchmark client process");
    assert!(
        pin.status.success(),
        "client taskset failed: {}",
        String::from_utf8_lossy(&pin.stderr)
    );
    client_affinity
}

fn unique_root() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "fr_io_uring_submission_ab_{}_{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create unique A/B root");
    root
}

fn hash_path(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("run sha256sum");
    assert!(
        output.status.success(),
        "sha256sum failed for {}",
        path.display()
    );
    String::from_utf8(output.stdout)
        .expect("sha256sum output is UTF-8")
        .split_whitespace()
        .next()
        .expect("sha256sum emitted digest")
        .to_owned()
}

fn parse_usize_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .map(|value| {
            value
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid {name}"))
        })
        .unwrap_or(default)
}

fn parse_client_thread_counts() -> Vec<usize> {
    let counts = match std::env::var("FR_URING_AB_CLIENT_THREAD_SWEEP") {
        Ok(value) => value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| {
                item.parse::<usize>().unwrap_or_else(|_| {
                    panic!("invalid FR_URING_AB_CLIENT_THREAD_SWEEP item: {item}")
                })
            })
            .collect::<Vec<_>>(),
        Err(std::env::VarError::NotPresent) => vec![parse_usize_env(
            "FR_URING_AB_CLIENT_THREADS",
            DEFAULT_CLIENT_THREADS,
        )],
        Err(error) => panic!("invalid FR_URING_AB_CLIENT_THREAD_SWEEP: {error}"),
    };
    assert!(!counts.is_empty(), "client thread sweep cannot be empty");
    assert!(
        counts
            .iter()
            .all(|count| (1..=MAX_CLIENT_THREADS).contains(count)),
        "client thread counts must be in 1..={MAX_CLIENT_THREADS}: {counts:?}"
    );
    assert!(
        counts.windows(2).all(|pair| pair[0] < pair[1]),
        "client thread sweep must be strictly increasing: {counts:?}"
    );
    counts
}

fn parse_sharded_server_thread_counts() -> Vec<usize> {
    let value = std::env::var("FR_SHARDED_SERVER_THREAD_SWEEP")
        .unwrap_or_else(|_| "1,2,4,8,16,32,64,128".to_owned());
    let counts = value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid FR_SHARDED_SERVER_THREAD_SWEEP item: {item}"))
        })
        .collect::<Vec<_>>();
    assert!(
        !counts.is_empty(),
        "sharded server thread sweep cannot be empty"
    );
    assert!(
        counts.iter().all(|count| (1..=128).contains(count)),
        "sharded server thread counts must be in 1..=128: {counts:?}"
    );
    assert!(
        counts.windows(2).all(|pair| pair[0] < pair[1]),
        "sharded server thread sweep must be strictly increasing: {counts:?}"
    );
    counts
}

fn parse_sharded_workloads() -> Vec<Workload> {
    let value = std::env::var("FR_SHARDED_WORKLOADS").unwrap_or_else(|_| "set,mixed".to_owned());
    let workloads = value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| match item {
            "set" => Workload::Set,
            "get" => Workload::Get,
            "mixed" => Workload::Mixed,
            "mixed-families" => Workload::MixedFamilies,
            other => panic!("unknown FR_SHARDED_WORKLOADS item: {other}"),
        })
        .collect::<Vec<_>>();
    assert!(
        !workloads.is_empty(),
        "sharded workload selection cannot be empty"
    );
    let mut unique = workloads.clone();
    unique.sort_unstable_by_key(|workload| workload.name());
    unique.dedup();
    assert_eq!(
        unique.len(),
        workloads.len(),
        "sharded workload selection contains duplicates: {workloads:?}"
    );
    workloads
}

fn balanced_shard_keys(connections: usize, workers: usize) -> Vec<Vec<u8>> {
    let mut keys = Vec::with_capacity(connections);
    let mut nonce = 0usize;
    for connection in 0..connections {
        let target = connection % workers;
        loop {
            let key = format!("mc:{target}:{nonce}").into_bytes();
            nonce += 1;
            if usize::from(fr_store::crc16_slot(&key)) % workers == target {
                keys.push(key);
                break;
            }
        }
    }
    let mut distribution = vec![0usize; workers];
    for key in &keys {
        distribution[usize::from(fr_store::crc16_slot(key)) % workers] += 1;
    }
    if connections >= workers {
        assert!(
            distribution.iter().all(|count| *count > 0),
            "balanced fixture must route work to every shard: {distribution:?}"
        );
    }
    println!(
        "SHARD_KEY_DISTRIBUTION workers={workers} connections={connections} \
distribution={distribution:?}"
    );
    keys
}

fn tagged_shard_keys(connections: usize, workers: usize, single_shard: bool) -> Vec<Vec<u8>> {
    let target_count = if single_shard { 1 } else { workers };
    let mut nonce = 0usize;
    let mut tags = Vec::with_capacity(target_count);
    for target in 0..target_count {
        loop {
            let tag = format!("mf:{target}:{nonce}");
            nonce += 1;
            if usize::from(fr_store::crc16_slot(tag.as_bytes())) % workers == target {
                tags.push(tag);
                break;
            }
        }
    }
    let keys = (0..connections)
        .map(|connection| {
            let target = if single_shard {
                0
            } else {
                connection % workers
            };
            format!("{{{}}}:connection:{connection}", tags[target]).into_bytes()
        })
        .collect::<Vec<_>>();
    let mut distribution = vec![0usize; workers];
    for key in &keys {
        distribution[usize::from(fr_store::crc16_slot(key)) % workers] += 1;
    }
    if single_shard {
        assert_eq!(
            distribution.iter().filter(|count| **count > 0).count(),
            1,
            "single-hot-shard fixture must route every key to one shard: {distribution:?}"
        );
    } else if connections >= workers {
        assert!(
            distribution.iter().all(|count| *count > 0),
            "independent mixed-family fixture must route work to every shard: {distribution:?}"
        );
    }
    println!(
        "MIXED_FAMILY_SHARD_KEY_DISTRIBUTION workers={workers} connections={connections} \
shape={} distribution={distribution:?}",
        if single_shard {
            "single_hot_shard_distinct_keys"
        } else {
            "independent_keys"
        }
    );
    keys
}

#[test]
fn zrangebyscore_range_packet_has_exact_inclusive_reply() {
    let single_reply = zrangebyscore_range_reply();
    assert_eq!(single_reply.len(), 11_018);
    assert!(single_reply.starts_with(b"*1001\r\n$5\r\n00500\r\n"));
    assert!(single_reply.ends_with(b"$5\r\n01500\r\n"));
    assert!(
        !single_reply
            .windows(b"00499".len())
            .any(|window| window == b"00499")
    );
    assert!(
        !single_reply
            .windows(b"01501".len())
            .any(|window| window == b"01501")
    );

    let packets = WorkloadPackets::new(Workload::ZrangebyscoreRange, 2);
    let mut expected_request = ZRANGEBYSCORE_RANGE.to_vec();
    expected_request.extend_from_slice(ZRANGEBYSCORE_RANGE);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn zrevrangebyscore_range_packet_has_exact_descending_reply() {
    let single_reply = zrevrangebyscore_range_reply();
    assert_eq!(single_reply.len(), 11_018);
    assert!(single_reply.starts_with(b"*1001\r\n$5\r\n01500\r\n"));
    assert!(single_reply.ends_with(b"$5\r\n00500\r\n"));
    assert!(
        !single_reply
            .windows(b"00499".len())
            .any(|window| window == b"00499")
    );
    assert!(
        !single_reply
            .windows(b"01501".len())
            .any(|window| window == b"01501")
    );

    let packets = WorkloadPackets::new(Workload::ZrevrangebyscoreRange, 2);
    let mut expected_request = ZREVRANGEBYSCORE_RANGE.to_vec();
    expected_request.extend_from_slice(ZREVRANGEBYSCORE_RANGE);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn zrevrange_rank_range_packet_has_exact_descending_reply() {
    let single_reply = zrevrange_rank_range_reply();
    assert_eq!(single_reply.len(), 11_018);
    assert!(single_reply.starts_with(b"*1001\r\n$5\r\n01500\r\n"));
    assert!(single_reply.ends_with(b"$5\r\n00500\r\n"));
    assert!(
        !single_reply
            .windows(b"00499".len())
            .any(|window| window == b"00499")
    );
    assert!(
        !single_reply
            .windows(b"01501".len())
            .any(|window| window == b"01501")
    );

    let packets = WorkloadPackets::new(Workload::ZrevrangeRankRange, 2);
    let mut expected_request = ZREVRANGE_RANK_RANGE.to_vec();
    expected_request.extend_from_slice(ZREVRANGE_RANK_RANGE);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn lpop_count_missing_packet_has_exact_null_array_reply() {
    let packets = WorkloadPackets::new(Workload::LpopCountMissing, 2);
    let mut expected_request = LPOP_COUNT_MISSING.to_vec();
    expected_request.extend_from_slice(LPOP_COUNT_MISSING);
    let mut expected_response = LIST_COUNT_MISSING_REPLY.to_vec();
    expected_response.extend_from_slice(LIST_COUNT_MISSING_REPLY);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn rpop_count_missing_packet_has_exact_null_array_reply() {
    let packets = WorkloadPackets::new(Workload::RpopCountMissing, 2);
    let mut expected_request = RPOP_COUNT_MISSING.to_vec();
    expected_request.extend_from_slice(RPOP_COUNT_MISSING);
    let mut expected_response = LIST_COUNT_MISSING_REPLY.to_vec();
    expected_response.extend_from_slice(LIST_COUNT_MISSING_REPLY);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn zdiff_two_key_packet_has_exact_2048_member_reply() {
    let single_reply = zdiff_two_key_reply();
    assert!(single_reply.starts_with(b"*2048\r\n$1\r\n0\r\n"));
    assert!(single_reply.ends_with(b"$4\r\n2047\r\n"));
    assert!(
        !single_reply
            .windows(b"$4\r\n2048\r\n".len())
            .any(|window| window == b"$4\r\n2048\r\n")
    );

    let packets = WorkloadPackets::new(Workload::ZdiffTwoKey, 2);
    let mut expected_request = ZDIFF_TWO_KEY.to_vec();
    expected_request.extend_from_slice(ZDIFF_TWO_KEY);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn zinter_two_key_packet_has_exact_2048_member_reply() {
    let single_reply = zinter_two_key_reply();
    assert!(single_reply.starts_with(b"*2048\r\n$4\r\n2048\r\n"));
    assert!(single_reply.ends_with(b"$4\r\n4095\r\n"));
    assert!(
        !single_reply
            .windows(b"$4\r\n2047\r\n".len())
            .any(|window| window == b"$4\r\n2047\r\n")
    );

    let packets = WorkloadPackets::new(Workload::ZinterTwoKey, 2);
    let mut expected_request = ZINTER_TWO_KEY.to_vec();
    expected_request.extend_from_slice(ZINTER_TWO_KEY);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn sscan_cursor_zero_packet_has_exact_complete_reply() {
    let single_reply = sscan_cursor_zero_reply();
    assert!(single_reply.starts_with(b"*2\r\n$1\r\n0\r\n*16\r\n$1\r\n0\r\n"));
    assert!(single_reply.ends_with(b"$2\r\n15\r\n"));

    let packets = WorkloadPackets::new(Workload::SscanCursorZero, 2);
    let mut expected_request = SSCAN_CURSOR_ZERO.to_vec();
    expected_request.extend_from_slice(SSCAN_CURSOR_ZERO);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn hscan_cursor_zero_packet_has_exact_complete_reply() {
    let single_reply = hscan_cursor_zero_reply();
    assert!(single_reply.starts_with(b"*2\r\n$1\r\n0\r\n*32\r\n$3\r\nf00\r\n$3\r\nv00\r\n"));
    assert!(single_reply.ends_with(b"$3\r\nf15\r\n$3\r\nv15\r\n"));

    let packets = WorkloadPackets::new(Workload::HscanCursorZero, 2);
    let mut expected_request = HSCAN_CURSOR_ZERO.to_vec();
    expected_request.extend_from_slice(HSCAN_CURSOR_ZERO);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn zscan_cursor_zero_packet_has_exact_complete_reply() {
    let single_reply = zscan_cursor_zero_reply();
    assert!(single_reply.starts_with(b"*2\r\n$1\r\n0\r\n*32\r\n$3\r\nm00\r\n$1\r\n0\r\n"));
    assert!(single_reply.ends_with(b"$3\r\nm15\r\n$2\r\n15\r\n"));

    let packets = WorkloadPackets::new(Workload::ZscanCursorZero, 2);
    let mut expected_request = ZSCAN_CURSOR_ZERO.to_vec();
    expected_request.extend_from_slice(ZSCAN_CURSOR_ZERO);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn zrangebylex_range_packet_has_exact_inclusive_reply() {
    let single_reply = zrangebylex_range_reply();
    assert_eq!(single_reply.len(), 11_018);
    assert!(single_reply.starts_with(b"*1001\r\n$5\r\n00500\r\n"));
    assert!(single_reply.ends_with(b"$5\r\n01500\r\n"));
    assert!(
        !single_reply
            .windows(b"00499".len())
            .any(|window| window == b"00499")
    );
    assert!(
        !single_reply
            .windows(b"01501".len())
            .any(|window| window == b"01501")
    );

    let packets = WorkloadPackets::new(Workload::ZrangebylexRange, 2);
    let mut expected_request = ZRANGEBYLEX_RANGE.to_vec();
    expected_request.extend_from_slice(ZRANGEBYLEX_RANGE);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn zrevrangebylex_range_packet_has_exact_descending_reply() {
    let single_reply = zrevrangebylex_range_reply();
    assert_eq!(single_reply.len(), 11_018);
    assert!(single_reply.starts_with(b"*1001\r\n$5\r\n01500\r\n"));
    assert!(single_reply.ends_with(b"$5\r\n00500\r\n"));
    assert!(
        !single_reply
            .windows(b"00499".len())
            .any(|window| window == b"00499")
    );
    assert!(
        !single_reply
            .windows(b"01501".len())
            .any(|window| window == b"01501")
    );

    let packets = WorkloadPackets::new(Workload::ZrevrangebylexRange, 2);
    let mut expected_request = ZREVRANGEBYLEX_RANGE.to_vec();
    expected_request.extend_from_slice(ZREVRANGEBYLEX_RANGE);
    let mut expected_response = single_reply.clone();
    expected_response.extend_from_slice(&single_reply);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
}

#[test]
fn zremrangebyrank_noop_packet_has_exact_integer_reply() {
    let packets = WorkloadPackets::new(Workload::ZremrangebyrankNoop, 2);
    let mut expected_request = ZREMRANGEBYRANK_NOOP.to_vec();
    expected_request.extend_from_slice(ZREMRANGEBYRANK_NOOP);
    let mut expected_response = ZREMRANGEBYRANK_NOOP_REPLY.to_vec();
    expected_response.extend_from_slice(ZREMRANGEBYRANK_NOOP_REPLY);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
    assert_eq!(
        packets
            .even
            .request
            .windows(b"ZREMRANGEBYRANK".len())
            .filter(|window| *window == b"ZREMRANGEBYRANK")
            .count(),
        2
    );
}

#[test]
fn zremrangebylex_noop_packet_has_exact_integer_reply() {
    let packets = WorkloadPackets::new(Workload::ZremrangebylexNoop, 2);
    let mut expected_request = ZREMRANGEBYLEX_NOOP.to_vec();
    expected_request.extend_from_slice(ZREMRANGEBYLEX_NOOP);
    let mut expected_response = ZREMRANGEBYLEX_NOOP_REPLY.to_vec();
    expected_response.extend_from_slice(ZREMRANGEBYLEX_NOOP_REPLY);
    assert_eq!(packets.even.request, expected_request);
    assert_eq!(packets.even.response, expected_response);
    assert_eq!(packets.odd.request, packets.even.request);
    assert_eq!(packets.odd.response, packets.even.response);
    assert_eq!(
        packets
            .even
            .request
            .windows(b"ZREMRANGEBYLEX".len())
            .filter(|window| *window == b"ZREMRANGEBYLEX")
            .count(),
        2
    );
}

#[test]
fn mixed_family_packets_cover_every_supported_command_with_exact_replies() {
    const EVEN_REPLIES: &[u8] = b"+OK\r\n$1\r\n0\r\n:1\r\n:1\r\n:0\r\n\
$1\r\na\r\n$1\r\na\r\n+OK\r\n:0\r\n:1\r\n$1\r\n0\r\n$1\r\nb\r\n\
:1\r\n$1\r\nb\r\n:0\r\n$1\r\na\r\n";
    const ODD_REPLIES: &[u8] = b"+OK\r\n:0\r\n:1\r\n$1\r\n0\r\n$1\r\nb\r\n\
:1\r\n$1\r\nb\r\n:0\r\n+OK\r\n$1\r\n0\r\n:1\r\n:1\r\n:0\r\n\
$1\r\na\r\n$1\r\na\r\n$1\r\na\r\n";

    let keys = tagged_shard_keys(4, 2, false);
    let (packets, setup) = DriverPackets::keyed_mixed_families(16, &keys);
    assert_eq!(setup.len(), keys.len());
    for case in &setup {
        assert_eq!(case.response, b":1\r\n");
    }
    for connection_index in 0..keys.len() {
        let connection = packets.for_connection(connection_index);
        assert!(connection.setup.is_none());
        assert_eq!(connection.measured.even.response, EVEN_REPLIES);
        assert_eq!(connection.measured.odd.response, ODD_REPLIES);
        for case in [&connection.measured.even, &connection.measured.odd] {
            for (command, expected) in [
                ("SET", 2),
                ("GET", 2),
                ("INCR", 2),
                ("LPUSH", 2),
                ("LPOP", 2),
                ("HSET", 3),
                ("HGET", 3),
            ] {
                let marker = format!("${}\r\n{command}\r\n", command.len());
                let actual = case
                    .request
                    .windows(marker.len())
                    .filter(|window| *window == marker.as_bytes())
                    .count();
                assert_eq!(
                    actual, expected,
                    "{command} count diverged in mixed-family P16 packet"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires live frankenredis and vendored Redis executables; run explicitly"]
fn mixed_family_packets_match_sharded_server_and_live_redis() {
    let binary = std::env::var_os("FR_URING_FR_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_frankenredis")));
    let redis_binary = std::env::var_os("FR_URING_REDIS_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../legacy_redis_code/redis/src/redis-server")
        });
    assert!(binary.is_file(), "missing {}", binary.display());
    assert!(redis_binary.is_file(), "missing {}", redis_binary.display());

    let server_cpu = *allowed_cpus()
        .first()
        .expect("live mixed-family smoke requires one allowed CPU");
    let root = unique_root();
    let client_shape = ClientShape {
        connections: 8,
        driver_threads: 2,
    };
    let keys = tagged_shard_keys(client_shape.connections, 2, false);
    let (packets, setup) = DriverPackets::keyed_mixed_families(16, &keys);
    let packets = Arc::new(packets);

    for (arm, workers, force_io_uring_output) in
        [(Arm::IoUring, Some(2), true), (Arm::Redis, None, false)]
    {
        let mut server = Server::spawn_with_options(
            &binary,
            &redis_binary,
            arm,
            &root,
            &[server_cpu],
            client_shape,
            CommandFloorAb::None,
            workers,
            force_io_uring_output,
        );
        for case in &setup {
            exchange_one(&mut server, &case.request, &case.response);
        }
        let worker_cpu_before = workers.map(|_| server.sharded_worker_cpu_ns());
        server
            .clients
            .as_ref()
            .expect("live mixed-family clients initialized")
            .run(&packets, 32, false);
        if let (Some(expected), Some(before)) = (workers, worker_cpu_before) {
            server.assert_sharded_set_get_workers_reached_process(expected);
            server.assert_flag_reached_process();
            let after = server.sharded_worker_cpu_ns();
            assert_eq!(
                active_sharded_worker_count(&before, &after),
                expected,
                "balanced mixed-family smoke must execute on every candidate worker"
            );
        }
        println!(
            "MIXED_FAMILY_LIVE_PARITY arm={} pid={} sha256={} \
process_threads_observed={} command_execution_threads_actual={} \
connections_actual={} client_driver_threads_actual={} \
commands_per_pipeline=16 groups=32 full_response_bytes_asserted=true",
            arm.name(),
            server.pid(),
            server.executing_elf_sha256(),
            server.observed_thread_count(),
            workers.unwrap_or(1),
            client_shape.connections,
            client_shape.driver_threads
        );
    }
}

fn active_sharded_worker_count(before: &HashMap<u32, u64>, after: &HashMap<u32, u64>) -> usize {
    after
        .iter()
        .filter(|(tid, ticks)| **ticks > before.get(tid).copied().unwrap_or(0))
        .count()
}

fn parse_u64_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .map(|value| {
            value
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("invalid {name}"))
        })
        .unwrap_or(default)
}

fn parse_bool_env(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) if value == "1" => true,
        Ok(value) if value == "0" => false,
        Ok(value) => panic!("{name} must be 0 or 1, got {value:?}"),
        Err(std::env::VarError::NotPresent) => false,
        Err(error) => panic!("invalid {name}: {error}"),
    }
}

fn parse_pipelines() -> Vec<usize> {
    if std::env::var_os("FR_URING_AB_P1_ONLY").is_some() {
        return vec![1];
    }
    let value = std::env::var("FR_URING_AB_PIPELINES").unwrap_or_else(|_| "1".to_owned());
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid pipeline depth: {item}"))
        })
        .collect()
}

fn command_output(command: &str, args: &[&str]) -> Output {
    Command::new(command)
        .args(args)
        .output()
        .unwrap_or_else(|_| panic!("run {command}"))
}

#[test]
fn sharded_profile_targets_match_current_rust_demangling() {
    let report = "\
     3.67%  [.] frankenredis::dispatch_sharded_set_get_frames
     1.26%  [.] frankenredis::drain_sharded_set_get_completions
     0.63%  [.] <fr_runtime::Runtime>::execute_plain_get_borrowed_into_with_default_read_gate
     0.55%  [.] <fr_runtime::Runtime>::execute_plain_set_borrowed_with_default_write_gate
     0.22%  [.] <fr_store::Store>::set_plain_borrowed
     0.11%  [.] <fr_store::Store>::get_string_bytes
    9.99%  [.] unrelated::hot_path";
    let (self_pct, rows) = matching_profile_self_time(report, &SHARDED_PROFILE_TARGETS);
    assert!((self_pct - 6.44).abs() < 1.0e-12);
    assert_eq!(rows.len(), SHARDED_PROFILE_TARGETS.len());
}

#[test]
fn median_ci_gate_accepts_precise_non_straddling_null_with_small_bias() {
    let null = [1.005_f64; 24];
    let candidate = [1.20_f64; 24];
    assert_eq!(
        adjudicate_ratios(
            "wall_ns_per_op",
            "candidate_over_redis",
            Workload::Set,
            16,
            128,
            &null,
            &candidate,
        ),
        Verdict::Keep
    );
}

#[test]
fn dual_null_gate_accepts_precise_non_straddling_incumbent_null_with_small_bias() {
    let candidate_null = [1.0_f64; 24];
    let redis_null = [1.005_f64; 24];
    let candidate_over_redis = [1.20_f64; 24];
    assert_eq!(
        adjudicate_dual_null_ratios(
            "wall_ns_per_op",
            "candidate_over_redis",
            Workload::Set,
            16,
            128,
            DualNullRatioSamples {
                candidate_null: &candidate_null,
                redis_null: &redis_null,
                ratio: &candidate_over_redis,
            },
        ),
        Verdict::Keep
    );
}

#[test]
#[should_panic(expected = "must remain within the 2% gross-bias guard")]
fn median_ci_gate_rejects_grossly_biased_null() {
    let null = [1.03_f64; 24];
    let candidate = [1.20_f64; 24];
    let _ = adjudicate_ratios(
        "wall_ns_per_op",
        "candidate_over_redis",
        Workload::Set,
        16,
        128,
        &null,
        &candidate,
    );
}

#[test]
#[should_panic(expected = "must each remain within the 2% gross-bias guard")]
fn dual_null_gate_rejects_grossly_biased_incumbent_null() {
    let candidate_null = [1.0_f64; 24];
    let redis_null = [1.03_f64; 24];
    let candidate_over_redis = [1.20_f64; 24];
    let _ = adjudicate_dual_null_ratios(
        "wall_ns_per_op",
        "candidate_over_redis",
        Workload::Set,
        16,
        128,
        DualNullRatioSamples {
            candidate_null: &candidate_null,
            redis_null: &redis_null,
            ratio: &candidate_over_redis,
        },
    );
}

#[test]
fn dual_null_gate_uses_the_wider_null_margin() {
    let candidate_null = [
        0.99_f64, 1.01, 0.99, 1.01, 0.99, 1.01, 0.99, 1.01, 0.99, 1.01, 0.99, 1.01, 0.99, 1.01,
        0.99, 1.01, 0.99, 1.01, 0.99, 1.01, 0.99, 1.01, 0.99, 1.01,
    ];
    let redis_null = [1.0_f64; 24];
    let candidate_over_redis = [1.015_f64; 24];
    assert_eq!(
        adjudicate_dual_null_ratios(
            "wall_ns_per_op",
            "candidate_over_redis",
            Workload::Set,
            16,
            128,
            DualNullRatioSamples {
                candidate_null: &candidate_null,
                redis_null: &redis_null,
                ratio: &candidate_over_redis,
            },
        ),
        Verdict::Hold
    );
}

#[test]
fn host_wide_quiescence_accepts_only_complete_quiet_cpuset() {
    let allowed = [0, 1, 2];
    let quiet = HashMap::from([(0, 0.0), (1, 10.0), (2, HOST_WIDE_MAX_BUSY_PCT)]);
    assert!(host_wide_quiescence_violations(&allowed, &quiet).is_empty());

    let contaminated = HashMap::from([(0, 0.0), (1, HOST_WIDE_MAX_BUSY_PCT + 0.1), (4, 0.0)]);
    let violations = host_wide_quiescence_violations(&allowed, &contaminated);
    assert_eq!(violations.len(), 2);
    assert!(violations.iter().any(|row| row.starts_with("cpu1=")));
    assert!(violations.iter().any(|row| row == "cpu2=missing"));
}

#[test]
fn dominant_libc_leaf_offsets_reach_specific_inlined_callers() {
    let report = "\
    18.21%  frankenredis  libc.so.6     [.] __memcmp_avx2_movbe
     6.00%  frankenredis  frankenredis  [.] command_surface";
    assert_eq!(aggregate_libc_self_pct(report), 18.21);

    let script = "\
         1
  7f00 [unknown] (/usr/lib/libc.so.6+0x1983a4)
  5500 caller_a (/build/release-perf/frankenredis+0x6e7351)
  5600 caller_b (/build/release-perf/frankenredis+0x6df34b)
         1
  7f10 [unknown] (/usr/lib/libc.so.6+0x1986a1)
  5700 caller_a (/build/release-perf/frankenredis+0x6e7351)
         1
  7f20 epoll_wait (/usr/lib/libc.so.6+0x1376d4)
  5800 caller_c (/build/release-perf/frankenredis+0x735cce)";
    let callers =
        dominant_libc_leaf_callers(script, Path::new("/proc/123/exe/frankenredis")).unwrap();
    assert_eq!(callers.identity, "unresolved_libc_page:0x198000");
    assert_eq!(callers.samples, 2);
    assert_eq!(callers.total_samples, 3);
    assert_eq!(callers.leaf_offsets, [0x1983a4, 0x1986a1]);
    assert_eq!(callers.caller_counts, [(0x6e7351, 2), (0x6df34b, 1)]);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zrangebyscore_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZrangebyscoreRange);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zrevrangebyscore_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZrevrangebyscoreRange);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zrevrange_rank_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZrevrangeRankRange);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zrangebylex_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZrangebylexRange);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zrevrangebylex_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZrevrangebylexRange);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zremrangebyrank_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZremrangebyrankNoop);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zremrangebylex_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZremrangebylexNoop);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn lpop_count_missing_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::LpopCountMissing);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn rpop_count_missing_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::RpopCountMissing);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zdiff_two_key_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZdiffTwoKey);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zinter_two_key_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZinterTwoKey);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn sscan_cursor_zero_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::SscanCursorZero);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn hscan_cursor_zero_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::HscanCursorZero);
}

#[test]
#[ignore = "strict-remote dual-null live-incumbent performance gate; run explicitly"]
fn zscan_cursor_zero_dual_null_vs_redis() {
    run_exact_dual_null_vs_redis(Workload::ZscanCursorZero);
}

fn run_exact_dual_null_vs_redis(workload: Workload) {
    assert!(
        matches!(
            workload,
            Workload::ZremrangebyrankNoop
                | Workload::ZremrangebylexNoop
                | Workload::ZrangebyscoreRange
                | Workload::ZrevrangebyscoreRange
                | Workload::ZrevrangeRankRange
                | Workload::ZrangebylexRange
                | Workload::ZrevrangebylexRange
                | Workload::LpopCountMissing
                | Workload::RpopCountMissing
                | Workload::ZdiffTwoKey
                | Workload::ZinterTwoKey
                | Workload::SscanCursorZero
                | Workload::HscanCursorZero
                | Workload::ZscanCursorZero
        ),
        "competitive gate requires an authenticated exact workload"
    );
    let binary = std::env::var_os("FR_URING_FR_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_frankenredis")));
    let redis_binary = std::env::var_os("FR_URING_REDIS_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../legacy_redis_code/redis/src/redis-server")
        });
    assert!(binary.is_file(), "missing {}", binary.display());
    assert!(redis_binary.is_file(), "missing {}", redis_binary.display());

    let harness = std::env::current_exe().expect("locate running harness ELF");
    println!(
        "HARNESS_ELF_SELF_REPORT sha256={} \
arms=candidate_a,candidate_b,vendored_redis_7.2.4_a,vendored_redis_7.2.4_b",
        hash_path(&harness)
    );
    println!(
        "DECISION_CONTRACT same_invocation=true independent_aa_per_arm=true \
candidate_aa=true incumbent_aa=true live_redis_arm=true \
bootstrap_median_ci_gate=true widest_null_margin=true \
null_median_bias_guard_pct=2 null_ci_straddle_telemetry_only=true \
cv_provenance_only=true never_cv_gate=true"
    );

    let redis_version = command_output(
        redis_binary
            .to_str()
            .expect("vendored Redis executable path must be UTF-8"),
        &["--version"],
    );
    assert!(
        redis_version.status.success(),
        "vendored Redis --version must succeed"
    );
    let redis_version = String::from_utf8(redis_version.stdout).expect("Redis version is UTF-8");
    assert!(
        redis_version.contains("v=7.2.4"),
        "expected vendored Redis 7.2.4, got {redis_version:?}"
    );
    println!("INCUMBENT_VERSION {}", redis_version.trim());

    let perf_version = command_output("perf", &["--version"]);
    assert!(
        perf_version.status.success(),
        "worker must provide perf for profile attribution"
    );
    let hostname = command_output("hostname", &[]);
    assert!(hostname.status.success(), "hostname failed");
    let hostname = String::from_utf8_lossy(&hostname.stdout).trim().to_owned();
    let kernel = command_output("uname", &["-r"]);
    assert!(kernel.status.success(), "uname failed");
    let kernel = String::from_utf8_lossy(&kernel.stdout).trim().to_owned();

    let env_prefix = if matches!(workload, Workload::ZremrangebyrankNoop) {
        "FR_ZREMRANGEBYRANK_AB"
    } else if matches!(workload, Workload::ZremrangebylexNoop) {
        "FR_ZREMRANGEBYLEX_AB"
    } else if matches!(workload, Workload::ZrangebyscoreRange) {
        "FR_ZRANGEBYSCORE_AB"
    } else if matches!(workload, Workload::ZrevrangebyscoreRange) {
        "FR_ZREVRANGEBYSCORE_AB"
    } else if matches!(workload, Workload::ZrevrangeRankRange) {
        "FR_ZREVRANGE_AB"
    } else if matches!(workload, Workload::LpopCountMissing) {
        "FR_LPOP_COUNT_AB"
    } else if matches!(workload, Workload::RpopCountMissing) {
        "FR_RPOP_COUNT_AB"
    } else if matches!(workload, Workload::ZdiffTwoKey) {
        "FR_ZDIFF_AB"
    } else if matches!(workload, Workload::ZinterTwoKey) {
        "FR_ZINTER_AB"
    } else if matches!(workload, Workload::SscanCursorZero) {
        "FR_SSCAN0_AB"
    } else if matches!(workload, Workload::HscanCursorZero) {
        "FR_HSCAN0_AB"
    } else if matches!(workload, Workload::ZscanCursorZero) {
        "FR_ZSCAN0_AB"
    } else {
        "FR_ZRANGEBYLEX_AB"
    };
    let samples_env = format!("{env_prefix}_SAMPLES");
    let clients_env = format!("{env_prefix}_CLIENTS");
    let client_threads_env = format!("{env_prefix}_CLIENT_THREADS");
    let pipeline_env = format!("{env_prefix}_PIPELINE");
    let ops_per_sample_env = format!("{env_prefix}_OPS_PER_SAMPLE");
    let interleave_groups_env = format!("{env_prefix}_INTERLEAVE_GROUPS");
    let profile_seconds_env = format!("{env_prefix}_PROFILE_SECONDS");
    let samples = parse_usize_env(&samples_env, DEFAULT_SAMPLES);
    let clients = parse_usize_env(&clients_env, DEFAULT_CLIENTS);
    let client_threads = parse_usize_env(&client_threads_env, DEFAULT_CLIENT_THREADS);
    let pipeline = parse_usize_env(&pipeline_env, 16);
    let ops_per_sample = parse_usize_env(&ops_per_sample_env, 1_600);
    let interleave_groups = parse_usize_env(&interleave_groups_env, 1);
    let profile_seconds = parse_u64_env(&profile_seconds_env, DEFAULT_PROFILE_SECONDS);
    assert!(samples >= 8, "median CI requires at least eight samples");
    assert!(
        samples.is_multiple_of(24),
        "sample count must contain complete 24-order cycles"
    );
    assert!(
        (1..=clients).contains(&client_threads),
        "actual client driver threads must be in 1..={clients}"
    );
    assert!(pipeline > 0, "pipeline depth must be positive");

    let (physical_cores, logical_threads) = machine_topology();
    let ram_kib = machine_ram_kib();
    let numa_nodes = machine_numa_nodes();
    let (client_cpu_order, server_core, process_cpuset_cap) =
        choose_client_cpu_order(client_threads);
    assert_eq!(
        process_cpuset_cap.len(),
        logical_threads,
        "benchmark process must retain the worker's complete logical CPU set"
    );
    let initial_cpu_frequency_policy = cpu_frequency_policy(&process_cpuset_cap);
    let initial_policy_phase = format!("{}_initial_pre_measurement", workload.name());
    print_cpu_frequency_policy(
        &initial_policy_phase,
        &process_cpuset_cap,
        &initial_cpu_frequency_policy,
    );
    let initial_quiescence_phase = format!("{}_initial_pre_pin", workload.name());
    assert_host_wide_quiescence(&process_cpuset_cap, &initial_quiescence_phase);
    let client_affinity = pin_client_process(&client_cpu_order, client_threads);
    println!(
        "WORKER_ID host={hostname} kernel={kernel} \
physical_cores={physical_cores} logical_threads={logical_threads} \
ram_kib={ram_kib} numa_nodes={numa_nodes} \
runtime_detected_isa={} process_cpuset_cap={process_cpuset_cap:?} \
server_affinity_cpu={server_core} client_affinity={client_affinity:?}",
        runtime_isa_features()
    );

    let root = unique_root();
    println!("ARTIFACT_ROOT {}", root.display());
    let client_shape = ClientShape {
        connections: clients,
        driver_threads: client_threads,
    };
    let mut servers = [
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::MioA,
            &root.join("candidate_a"),
            server_core,
            client_shape,
            CommandFloorAb::None,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::MioB,
            &root.join("candidate_b"),
            server_core,
            client_shape,
            CommandFloorAb::None,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::Redis,
            &root.join("redis_a"),
            server_core,
            client_shape,
            CommandFloorAb::None,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::Redis,
            &root.join("redis_b"),
            server_core,
            client_shape,
            CommandFloorAb::None,
        ),
    ];
    let server_hashes: [String; 4] =
        std::array::from_fn(|index| servers[index].executing_elf_sha256());
    assert_eq!(
        server_hashes[0], server_hashes[1],
        "candidate A/A arms must execute the same FrankenRedis ELF"
    );
    assert_eq!(
        server_hashes[2], server_hashes[3],
        "incumbent A/A arms must execute the same Redis ELF"
    );
    for (role, index) in [
        ("candidate_a", 0),
        ("candidate_b", 1),
        ("redis_a", 2),
        ("redis_b", 3),
    ] {
        assert_eq!(
            servers[index].client_thread_count(),
            client_threads,
            "{role} client driver thread count diverged"
        );
        println!(
            "SERVER_ELF_SELF_REPORT arm={role} pid={} sha256={} \
process_threads_observed={} command_execution_threads_actual=1 \
client_driver_threads_actual={} client_connections_actual={} affinity_cpus={:?}",
            servers[index].pid(),
            server_hashes[index],
            servers[index].observed_thread_count(),
            servers[index].client_thread_count(),
            clients,
            servers[index].affinity_cpus()
        );
    }
    println!(
        "ARM_SEMANTICS candidate_a=frankenredis_default_mio \
candidate_b=frankenredis_default_mio incumbent_a=vendored_redis_7.2.4 \
incumbent_b=vendored_redis_7.2.4"
    );

    prefill(&mut servers, workload);
    profile_io_uring_path(
        &mut servers[0],
        &root,
        profile_seconds,
        workload,
        pipeline,
        client_threads,
    );
    let packets = Arc::new(DriverPackets::shared(workload, pipeline));
    let workload_shape = if matches!(workload, Workload::ZremrangebyrankNoop) {
        "equal_score_2000_member_skiplist_out_of_range_rank_delete_noop"
    } else if matches!(workload, Workload::ZremrangebylexNoop) {
        "equal_score_2000_member_skiplist_out_of_range_lex_delete_noop"
    } else if matches!(workload, Workload::ZrangebyscoreRange) {
        "distinct_score_2000_member_skiplist_inclusive_1001_member_ascending_range"
    } else if matches!(workload, Workload::ZrevrangebyscoreRange) {
        "distinct_score_2000_member_skiplist_inclusive_1001_member_descending_range"
    } else if matches!(workload, Workload::ZrevrangeRankRange) {
        "distinct_score_2000_member_skiplist_reverse_rank_499_1499_1001_member_descending_range"
    } else if matches!(workload, Workload::LpopCountMissing) {
        "missing_list_key_count_2_exact_null_array_left_pop"
    } else if matches!(workload, Workload::RpopCountMissing) {
        "missing_list_key_count_2_exact_null_array_right_pop"
    } else if matches!(workload, Workload::ZdiffTwoKey) {
        "overlapping_4096_member_skiplist_sources_2048_member_difference"
    } else if matches!(workload, Workload::ZinterTwoKey) {
        "overlapping_4096_member_skiplist_sources_2048_member_intersection"
    } else if matches!(workload, Workload::SscanCursorZero) {
        "literal_cursor_zero_no_options_16_member_intset_single_complete_reply"
    } else if matches!(
        workload,
        Workload::HscanCursorZero | Workload::ZscanCursorZero
    ) {
        "literal_cursor_zero_no_options_16_pair_listpack_single_complete_reply"
    } else if matches!(workload, Workload::ZrangebylexRange) {
        "equal_score_2000_member_inclusive_1001_member_ascending_range"
    } else {
        "equal_score_2000_member_inclusive_1001_member_descending_range"
    };
    let samples = measure_dual_null_configuration_with_packets(
        &mut servers,
        workload,
        pipeline,
        MeasurementConfig {
            client_shape,
            samples,
            ops_per_sample,
            interleave_groups,
        },
        packets,
        PacketMeasurement {
            prepare_keyed: false,
            workload_shape,
            host_wide_allowed_cpus: Some(&process_cpuset_cap),
        },
    );
    if matches!(
        workload,
        Workload::ZremrangebyrankNoop | Workload::ZremrangebylexNoop
    ) {
        for server in &mut servers {
            assert_lexz_skiplist_fixture(server);
        }
        println!(
            "POST_MEASUREMENT_STATE workload={} arms=4 cardinality=2000 \
encoding=skiplist pttl=-1 exact=true",
            workload.name()
        );
    } else if matches!(
        workload,
        Workload::ZrangebyscoreRange
            | Workload::ZrevrangebyscoreRange
            | Workload::ZrevrangeRankRange
    ) {
        for server in &mut servers {
            assert_scorez_skiplist_fixture(server);
        }
        println!(
            "POST_MEASUREMENT_STATE workload={} arms=4 cardinality=2000 \
encoding=skiplist pttl=-1 exact=true",
            workload.name()
        );
    } else if matches!(
        workload,
        Workload::LpopCountMissing | Workload::RpopCountMissing
    ) {
        let request = if matches!(workload, Workload::LpopCountMissing) {
            LPOP_COUNT_MISSING
        } else {
            RPOP_COUNT_MISSING
        };
        for server in &mut servers {
            exchange_one(server, request, LIST_COUNT_MISSING_REPLY);
            exchange_one(
                server,
                LIST_COUNT_MISSING_EXISTS,
                LIST_COUNT_MISSING_EXISTS_REPLY,
            );
            exchange_one(
                server,
                LIST_COUNT_MISSING_PTTL,
                LIST_COUNT_MISSING_PTTL_REPLY,
            );
        }
        println!(
            "POST_MEASUREMENT_STATE workload={} arms=4 key=missing-list \
key_exists=false pttl=-2 exact_null_array_reply=true",
            workload.name()
        );
    } else if matches!(workload, Workload::ZdiffTwoKey | Workload::ZinterTwoKey) {
        let (request, response, result_shape) = if matches!(workload, Workload::ZdiffTwoKey) {
            (
                ZDIFF_TWO_KEY,
                zdiff_two_key_reply(),
                "difference_members=0..2047",
            )
        } else {
            (
                ZINTER_TWO_KEY,
                zinter_two_key_reply(),
                "intersection_members=2048..4095",
            )
        };
        for server in &mut servers {
            exchange_one(server, ZINTERCARD_ZA_CARD, ZINTERCARD_SOURCE_CARD_REPLY);
            exchange_one(server, ZINTERCARD_ZB_CARD, ZINTERCARD_SOURCE_CARD_REPLY);
            exchange_one(
                server,
                ZINTERCARD_ZA_ENCODING,
                ZINTERCARD_SKIPLIST_ENCODING_REPLY,
            );
            exchange_one(
                server,
                ZINTERCARD_ZB_ENCODING,
                ZINTERCARD_SKIPLIST_ENCODING_REPLY,
            );
            exchange_one(
                server,
                ZINTERCARD_ZA_BOUNDARIES,
                ZINTERCARD_ZA_BOUNDARIES_REPLY,
            );
            exchange_one(
                server,
                ZINTERCARD_ZB_BOUNDARIES,
                ZINTERCARD_ZB_BOUNDARIES_REPLY,
            );
            exchange_one(server, ZINTERCARD_ZA_PTTL, PTTL_PERSISTENT_REPLY);
            exchange_one(server, ZINTERCARD_ZB_PTTL, PTTL_PERSISTENT_REPLY);
            exchange_one(server, request, &response);
        }
        println!(
            "POST_MEASUREMENT_STATE workload={} arms=4 source_a_members=4096 \
source_b_members=4096 source_a_encoding=skiplist source_b_encoding=skiplist \
source_pttl=-1 boundary_scores=verified result_shape={result_shape} \
full_response_bytes_asserted=true",
            workload.name()
        );
    } else if matches!(
        workload,
        Workload::SscanCursorZero | Workload::HscanCursorZero | Workload::ZscanCursorZero
    ) {
        for server in &mut servers {
            assert_scan0_fixture(server, workload);
        }
        let compact_encoding = if matches!(workload, Workload::SscanCursorZero) {
            "intset"
        } else {
            "listpack"
        };
        println!(
            "POST_MEASUREMENT_STATE workload={} arms=4 members={} \
literal_cursor=0 options=none default_count=10 returned_cursor=0 \
encoding={compact_encoding} pttl=-1 boundary_membership=verified \
full_response_bytes_asserted=true",
            workload.name(),
            SCAN0_MEMBERS
        );
    }
    let verdict = adjudicate_dual_null(workload, pipeline, client_threads, &samples);
    println!(
        "COMPETITIVE_VERDICT workload={} ratio_direction=FrankenRedis/Redis \
wall_verdict={:?} cpu_fixed_work_verdict={:?} \
claim_class=COMPETITIVE same_invocation=true",
        workload.name(),
        verdict.0,
        verdict.1
    );

    let final_cpu_frequency_policy = cpu_frequency_policy(&process_cpuset_cap);
    let final_policy_phase = format!("{}_final_post_measurement", workload.name());
    print_cpu_frequency_policy(
        &final_policy_phase,
        &process_cpuset_cap,
        &final_cpu_frequency_policy,
    );
    assert_eq!(
        initial_cpu_frequency_policy,
        final_cpu_frequency_policy,
        "CPU frequency policy changed during the {} invocation",
        workload.name()
    );
}

#[test]
#[ignore = "host-wide full server command-execution thread sweep; run explicitly"]
fn sharded_set_get_server_thread_sweep_same_invocation() {
    let binary = std::env::var_os("FR_URING_FR_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_frankenredis")));
    let redis_binary = std::env::var_os("FR_URING_REDIS_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../legacy_redis_code/redis/src/redis-server")
        });
    assert!(
        binary.is_file(),
        "FrankenRedis executable is missing: {}",
        binary.display()
    );
    assert!(
        redis_binary.is_file(),
        "vendored Redis executable is missing: {}",
        redis_binary.display()
    );
    let harness = std::env::current_exe().expect("locate running harness ELF");
    println!(
        "HARNESS_ELF_SELF_REPORT sha256={} \
arms=sharded_candidate_a,sharded_candidate_b,vendored_redis_7.2.4_a,\
vendored_redis_7.2.4_b",
        hash_path(&harness)
    );
    println!(
        "DECISION_CONTRACT same_invocation=true independent_aa_per_arm=true \
candidate_aa=true incumbent_aa=true live_redis_arm=true \
bootstrap_median_ci_gate=true widest_null_margin=true \
null_median_bias_guard_pct=2 null_ci_straddle_telemetry_only=true \
cv_provenance_only=true never_cv_gate=true"
    );

    let redis_version = command_output(
        redis_binary
            .to_str()
            .expect("vendored Redis executable path must be UTF-8"),
        &["--version"],
    );
    assert!(
        redis_version.status.success(),
        "vendored Redis --version must succeed"
    );
    let redis_version = String::from_utf8(redis_version.stdout).expect("Redis version is UTF-8");
    assert!(
        redis_version.contains("v=7.2.4"),
        "expected vendored Redis 7.2.4, got {redis_version:?}"
    );
    println!("INCUMBENT_VERSION {}", redis_version.trim());

    let hostname = command_output("hostname", &[]);
    assert!(hostname.status.success(), "hostname failed");
    let hostname = String::from_utf8_lossy(&hostname.stdout).trim().to_owned();
    let (physical_cores, logical_threads) = machine_topology();
    let ram_kib = machine_ram_kib();
    let numa_nodes = machine_numa_nodes();
    let allow_remote_host = parse_bool_env("FR_SHARDED_ALLOW_REMOTE_HOST");
    if allow_remote_host {
        assert!(
            physical_cores >= 4 && logical_threads >= 4,
            "remote host must expose at least four physical/logical CPUs: \
host={hostname} physical={physical_cores} logical={logical_threads}"
        );
    } else {
        assert_eq!(
            hostname, "threadripperje",
            "set FR_SHARDED_ALLOW_REMOTE_HOST=1 only for a strict-remote clean-overlay \
invocation that records the actual host and retains every host-wide guard"
        );
        assert_eq!(
            (physical_cores, logical_threads),
            (64, 128),
            "trj topology changed; do not publish ambiguous scaling evidence"
        );
    }
    println!(
        "SCALING_HOST_CONTRACT host_identity={hostname} \
trj_required={} strict_remote_host_mode={} \
physical_cores={physical_cores} logical_threads={logical_threads}",
        !allow_remote_host, allow_remote_host
    );
    let server_thread_counts = parse_sharded_server_thread_counts();
    let workloads = parse_sharded_workloads();
    let samples = parse_usize_env("FR_SHARDED_SAMPLES", DEFAULT_SAMPLES);
    assert!(samples >= 24, "sharded sweep requires at least 24 samples");
    assert!(
        samples.is_multiple_of(24),
        "sharded sweep requires complete 24-order cycles"
    );
    let clients = parse_usize_env("FR_SHARDED_CLIENTS", 128);
    let client_threads = parse_usize_env("FR_SHARDED_CLIENT_THREADS", 128);
    assert!(
        (1..=clients).contains(&client_threads),
        "client driver threads must be in 1..={clients}"
    );
    let pipeline = parse_usize_env("FR_SHARDED_PIPELINE", 16);
    assert_eq!(
        pipeline, 16,
        "q01r6 sharded scaling is pre-registered at pipeline depth 16"
    );
    let ops_per_sample = parse_usize_env("FR_SHARDED_OPS_PER_SAMPLE", DEFAULT_OPS_PER_SAMPLE);
    let interleave_groups = parse_usize_env(
        "FR_SHARDED_INTERLEAVE_GROUPS",
        SHARDED_DEFAULT_INTERLEAVE_GROUPS,
    );
    let groups_per_arm_sample = ops_per_sample.div_ceil(clients * pipeline).max(1);
    assert!(
        interleave_groups < groups_per_arm_sample,
        "sharded sweep must rotate arms within each sample: \
interleave_groups={interleave_groups} groups_per_arm_sample={groups_per_arm_sample}"
    );
    let profile_seconds = parse_u64_env("FR_SHARDED_PROFILE_SECONDS", DEFAULT_PROFILE_SECONDS);
    let (client_cpu_order, server_core, process_cpuset_cap) =
        choose_client_cpu_order(client_threads);
    assert_eq!(
        process_cpuset_cap.len(),
        logical_threads,
        "host-wide scaling requires access to every logical CPU; \
process cpuset has {} of {logical_threads}",
        process_cpuset_cap.len()
    );
    let initial_cpu_frequency_policy = cpu_frequency_policy(&process_cpuset_cap);
    print_cpu_frequency_policy(
        "initial_pre_measurement",
        &process_cpuset_cap,
        &initial_cpu_frequency_policy,
    );
    assert_host_wide_quiescence(&process_cpuset_cap, "initial_pre_pin");
    let client_affinity = pin_client_process(&client_cpu_order, client_threads);
    println!(
        "SCALING_HARDWARE_PROVENANCE host_identity={hostname} \
physical_cores={physical_cores} logical_threads={logical_threads} \
ram_kib={ram_kib} numa_nodes={numa_nodes} \
server_thread_counts_requested={server_thread_counts:?} \
workloads_requested={workloads:?} \
client_driver_threads_actual={client_threads} client_connections={clients} \
runtime_detected_isa={} process_cpuset_cap={process_cpuset_cap:?} \
client_affinity={client_affinity:?} redis_affinity_cpu={server_core} \
candidate_a_affinity={process_cpuset_cap:?} candidate_b_affinity={process_cpuset_cap:?}",
        runtime_isa_features()
    );
    let root = unique_root();
    println!("ARTIFACT_ROOT {}", root.display());
    let client_shape = ClientShape {
        connections: clients,
        driver_threads: client_threads,
    };
    let measurement = MeasurementConfig {
        client_shape,
        samples,
        ops_per_sample,
        interleave_groups,
    };
    let mut verdicts = Vec::new();

    for server_workers in server_thread_counts {
        let point_root = root.join(format!("server_workers_{server_workers}"));
        fs::create_dir_all(&point_root).expect("create server-thread point root");
        let candidate_a_root = point_root.join("candidate_a");
        let candidate_b_root = point_root.join("candidate_b");
        let redis_a_root = point_root.join("redis_a");
        let redis_b_root = point_root.join("redis_b");
        let mut servers = [
            Server::spawn_with_options(
                &binary,
                &redis_binary,
                Arm::IoUring,
                &candidate_a_root,
                &process_cpuset_cap,
                client_shape,
                CommandFloorAb::None,
                Some(server_workers),
                true,
            ),
            Server::spawn_with_options(
                &binary,
                &redis_binary,
                Arm::IoUring,
                &candidate_b_root,
                &process_cpuset_cap,
                client_shape,
                CommandFloorAb::None,
                Some(server_workers),
                true,
            ),
            Server::spawn_with_options(
                &binary,
                &redis_binary,
                Arm::Redis,
                &redis_a_root,
                &[server_core],
                client_shape,
                CommandFloorAb::None,
                None,
                false,
            ),
            Server::spawn_with_options(
                &binary,
                &redis_binary,
                Arm::Redis,
                &redis_b_root,
                &[server_core],
                client_shape,
                CommandFloorAb::None,
                None,
                false,
            ),
        ];
        let server_hashes: [String; 4] =
            std::array::from_fn(|index| servers[index].executing_elf_sha256());
        assert_eq!(
            server_hashes[DualNullArm::CandidateA.index()],
            server_hashes[DualNullArm::CandidateB.index()],
            "candidate A/A arms must execute the same FrankenRedis ELF"
        );
        assert_eq!(
            server_hashes[DualNullArm::RedisA.index()],
            server_hashes[DualNullArm::RedisB.index()],
            "incumbent A/A arms must execute the same Redis ELF"
        );
        for index in [
            DualNullArm::CandidateA.index(),
            DualNullArm::CandidateB.index(),
        ] {
            servers[index].assert_sharded_set_get_workers_reached_process(server_workers);
            servers[index].assert_flag_reached_process();
        }
        let candidate_worker_threads_observed = [
            servers[DualNullArm::CandidateA.index()]
                .sharded_worker_cpu_ns()
                .len(),
            servers[DualNullArm::CandidateB.index()]
                .sharded_worker_cpu_ns()
                .len(),
        ];
        assert!(
            candidate_worker_threads_observed
                .iter()
                .all(|observed| *observed == server_workers),
            "both candidate A/A processes must expose every requested command worker: \
requested={server_workers} observed={candidate_worker_threads_observed:?}"
        );
        for (role, index, command_execution_threads) in [
            (
                "sharded_candidate_a",
                DualNullArm::CandidateA.index(),
                server_workers,
            ),
            (
                "sharded_candidate_b",
                DualNullArm::CandidateB.index(),
                server_workers,
            ),
            ("redis_a", DualNullArm::RedisA.index(), 1),
            ("redis_b", DualNullArm::RedisB.index(), 1),
        ] {
            println!(
                "SERVER_ELF_SELF_REPORT candidate_sweep_threads={server_workers} \
arm={role} pid={} sha256={} process_threads_observed={} \
command_execution_threads_actual={command_execution_threads} affinity_cpus={:?}",
                servers[index].pid(),
                server_hashes[index],
                servers[index].observed_thread_count(),
                servers[index].affinity_cpus()
            );
        }
        println!(
            "SCALING_POINT_PROVENANCE host_identity={hostname} \
physical_cores={physical_cores} logical_threads={logical_threads} \
ram_kib={ram_kib} numa_nodes={numa_nodes} \
thread_count_actually_used={server_workers} \
candidate_a_command_execution_threads={server_workers} \
candidate_b_command_execution_threads={server_workers} \
candidate_worker_threads_spawned_actual={candidate_worker_threads_observed:?} \
incumbent_a_command_execution_threads=1 incumbent_b_command_execution_threads=1 \
client_driver_threads_actual={client_threads} client_connections={clients} \
runtime_detected_isa={} process_cpuset_cap={process_cpuset_cap:?} \
client_affinity={client_affinity:?} candidate_a_affinity={:?} \
candidate_b_affinity={:?} redis_a_affinity={:?} redis_b_affinity={:?} \
campaign_multicore_scaling_result=true",
            runtime_isa_features(),
            servers[DualNullArm::CandidateA.index()].affinity_cpus(),
            servers[DualNullArm::CandidateB.index()].affinity_cpus(),
            servers[DualNullArm::RedisA.index()].affinity_cpus(),
            servers[DualNullArm::RedisB.index()].affinity_cpus()
        );
        println!(
            "ARM_SEMANTICS candidate_a=key_sharded_supported_single_key_commands_io_uring \
candidate_b=key_sharded_supported_single_key_commands_io_uring \
incumbent_a=vendored_redis_7.2.4 incumbent_b=vendored_redis_7.2.4"
        );

        let independent_keys = balanced_shard_keys(clients, server_workers);
        for workload in workloads.iter().copied() {
            let (packets, server_setup) = if matches!(workload, Workload::MixedFamilies) {
                let family_keys = tagged_shard_keys(clients, server_workers, false);
                DriverPackets::keyed_mixed_families(pipeline, &family_keys)
            } else {
                (
                    DriverPackets::keyed(workload, pipeline, &independent_keys),
                    Vec::new(),
                )
            };
            apply_server_setup(&mut servers, &server_setup);
            let packets = Arc::new(packets);
            if matches!(workload, Workload::Mixed | Workload::MixedFamilies) {
                profile_sharded_set_get_path(
                    &mut servers[DualNullArm::CandidateA.index()],
                    &point_root,
                    profile_seconds,
                    workload,
                    pipeline,
                    server_workers,
                    &packets,
                );
            }
            if matches!(workload, Workload::MixedFamilies) {
                println!(
                    "MIXED_FAMILY_WORKLOAD_CONTRACT pipeline={pipeline} \
commands_per_pipeline=16 set=2 get=2 incr=2 lpush=2 lpop=2 hset=3 hget=3 \
distinct_type_keys_per_connection=3 same_shard_per_connection=true \
full_response_bytes_asserted=true state_closed_between_groups=true"
                );
            }
            let candidate_worker_cpu_ns_before = [
                servers[DualNullArm::CandidateA.index()].sharded_worker_cpu_ns(),
                servers[DualNullArm::CandidateB.index()].sharded_worker_cpu_ns(),
            ];
            let measured = measure_dual_null_configuration_with_packets(
                &mut servers,
                workload,
                pipeline,
                measurement,
                packets,
                PacketMeasurement {
                    prepare_keyed: true,
                    workload_shape: "independent_key_per_connection",
                    host_wide_allowed_cpus: Some(&process_cpuset_cap),
                },
            );
            let candidate_worker_cpu_ns_after = [
                servers[DualNullArm::CandidateA.index()].sharded_worker_cpu_ns(),
                servers[DualNullArm::CandidateB.index()].sharded_worker_cpu_ns(),
            ];
            let active_workers = [
                active_sharded_worker_count(
                    &candidate_worker_cpu_ns_before[0],
                    &candidate_worker_cpu_ns_after[0],
                ),
                active_sharded_worker_count(
                    &candidate_worker_cpu_ns_before[1],
                    &candidate_worker_cpu_ns_after[1],
                ),
            ];
            assert!(
                active_workers
                    .iter()
                    .all(|active| *active == server_workers),
                "independent-key fixture must execute on every requested shard in both \
candidate A/A arms: requested={server_workers} active={active_workers:?}"
            );
            println!(
                "COUNTED_MECHANISM workload={} workload_shape=independent_key_per_connection \
server_command_execution_threads={server_workers} \
candidate_a_active_command_workers={} candidate_b_active_command_workers={} \
thread_count_actually_used_candidate_a={} thread_count_actually_used_candidate_b={} \
semantic_commands_per_arm_sample={} queue_envelope_commands_per_same_shard_batch={pipeline}",
                workload.name(),
                active_workers[0],
                active_workers[1],
                active_workers[0],
                active_workers[1],
                groups_per_arm_sample * clients * pipeline
            );
            let verdict = adjudicate_dual_null(workload, pipeline, client_threads, &measured);
            println!(
                "SHARDED_SWEEP_VERDICT workload={} \
workload_shape=independent_key_per_connection \
server_command_execution_threads={server_workers} verdict={verdict:?}",
                workload.name()
            );
            verdicts.push((
                server_workers,
                workload.name(),
                "independent_key_per_connection",
                verdict,
            ));
        }

        if workloads.contains(&Workload::Mixed) {
            let hot_keys = vec![b"mc:hot-key".to_vec(); clients];
            let hot_packets = Arc::new(DriverPackets::keyed(Workload::Mixed, pipeline, &hot_keys));
            let candidate_worker_cpu_ns_before = [
                servers[DualNullArm::CandidateA.index()].sharded_worker_cpu_ns(),
                servers[DualNullArm::CandidateB.index()].sharded_worker_cpu_ns(),
            ];
            let hot_measured = measure_dual_null_configuration_with_packets(
                &mut servers,
                Workload::Mixed,
                pipeline,
                measurement,
                hot_packets,
                PacketMeasurement {
                    prepare_keyed: true,
                    workload_shape: "single_hot_key_control",
                    host_wide_allowed_cpus: Some(&process_cpuset_cap),
                },
            );
            let candidate_worker_cpu_ns_after = [
                servers[DualNullArm::CandidateA.index()].sharded_worker_cpu_ns(),
                servers[DualNullArm::CandidateB.index()].sharded_worker_cpu_ns(),
            ];
            let active_workers = [
                active_sharded_worker_count(
                    &candidate_worker_cpu_ns_before[0],
                    &candidate_worker_cpu_ns_after[0],
                ),
                active_sharded_worker_count(
                    &candidate_worker_cpu_ns_before[1],
                    &candidate_worker_cpu_ns_after[1],
                ),
            ];
            assert!(
                active_workers.iter().all(|active| *active == 1),
                "single-hot-key control must execute on exactly one shard in both candidate A/A \
arms: active={active_workers:?}"
            );
            println!(
                "COUNTED_MECHANISM workload=mixed workload_shape=single_hot_key_control \
server_command_execution_threads={server_workers} \
candidate_a_active_command_workers={} candidate_b_active_command_workers={} \
thread_count_actually_used_candidate_a={} thread_count_actually_used_candidate_b={} \
semantic_commands_per_arm_sample={} queue_envelope_commands_per_same_shard_batch={pipeline}",
                active_workers[0],
                active_workers[1],
                active_workers[0],
                active_workers[1],
                groups_per_arm_sample * clients * pipeline
            );
            let hot_verdict =
                adjudicate_dual_null(Workload::Mixed, pipeline, client_threads, &hot_measured);
            println!(
                "SHARDED_SWEEP_VERDICT workload=mixed workload_shape=single_hot_key_control \
server_command_execution_threads={server_workers} verdict={hot_verdict:?}"
            );
            verdicts.push((
                server_workers,
                Workload::Mixed.name(),
                "single_hot_key_control",
                hot_verdict,
            ));
        }
        if workloads.contains(&Workload::MixedFamilies) {
            let hot_shard_keys = tagged_shard_keys(clients, server_workers, true);
            let (hot_packets, server_setup) =
                DriverPackets::keyed_mixed_families(pipeline, &hot_shard_keys);
            apply_server_setup(&mut servers, &server_setup);
            let hot_packets = Arc::new(hot_packets);
            println!(
                "MIXED_FAMILY_HOT_CONTROL_CONTRACT \
workload_shape=single_hot_shard_distinct_keys_control \
connections={clients} distinct_keys_per_connection=3 \
all_keys_route_to_one_command_worker=true full_response_bytes_asserted=true"
            );
            let candidate_worker_cpu_ns_before = [
                servers[DualNullArm::CandidateA.index()].sharded_worker_cpu_ns(),
                servers[DualNullArm::CandidateB.index()].sharded_worker_cpu_ns(),
            ];
            let hot_measured = measure_dual_null_configuration_with_packets(
                &mut servers,
                Workload::MixedFamilies,
                pipeline,
                measurement,
                hot_packets,
                PacketMeasurement {
                    prepare_keyed: true,
                    workload_shape: "single_hot_shard_distinct_keys_control",
                    host_wide_allowed_cpus: Some(&process_cpuset_cap),
                },
            );
            let candidate_worker_cpu_ns_after = [
                servers[DualNullArm::CandidateA.index()].sharded_worker_cpu_ns(),
                servers[DualNullArm::CandidateB.index()].sharded_worker_cpu_ns(),
            ];
            let active_workers = [
                active_sharded_worker_count(
                    &candidate_worker_cpu_ns_before[0],
                    &candidate_worker_cpu_ns_after[0],
                ),
                active_sharded_worker_count(
                    &candidate_worker_cpu_ns_before[1],
                    &candidate_worker_cpu_ns_after[1],
                ),
            ];
            assert!(
                active_workers.iter().all(|active| *active == 1),
                "single-hot-shard mixed-family control must execute on exactly one shard in \
both candidate A/A arms: active={active_workers:?}"
            );
            println!(
                "COUNTED_MECHANISM workload=mixed-families \
workload_shape=single_hot_shard_distinct_keys_control \
server_command_execution_threads={server_workers} \
candidate_a_active_command_workers={} candidate_b_active_command_workers={} \
thread_count_actually_used_candidate_a={} thread_count_actually_used_candidate_b={} \
semantic_commands_per_arm_sample={} queue_envelope_commands_per_same_shard_batch={pipeline}",
                active_workers[0],
                active_workers[1],
                active_workers[0],
                active_workers[1],
                groups_per_arm_sample * clients * pipeline
            );
            let hot_verdict = adjudicate_dual_null(
                Workload::MixedFamilies,
                pipeline,
                client_threads,
                &hot_measured,
            );
            println!(
                "SHARDED_SWEEP_VERDICT workload=mixed-families \
workload_shape=single_hot_shard_distinct_keys_control \
server_command_execution_threads={server_workers} verdict={hot_verdict:?}"
            );
            verdicts.push((
                server_workers,
                Workload::MixedFamilies.name(),
                "single_hot_shard_distinct_keys_control",
                hot_verdict,
            ));
        }
    }
    let final_cpu_frequency_policy = cpu_frequency_policy(&process_cpuset_cap);
    print_cpu_frequency_policy(
        "final_post_measurement",
        &process_cpuset_cap,
        &final_cpu_frequency_policy,
    );
    assert_eq!(
        initial_cpu_frequency_policy, final_cpu_frequency_policy,
        "CPU frequency policy changed during the sharded scaling invocation"
    );
    println!("SHARDED_SERVER_THREAD_VERDICT_MATRIX {verdicts:?}");
}

#[test]
#[ignore = "strict-remote pinned-worker performance gate; run explicitly"]
fn io_uring_submission_same_elf_null_then_ab() {
    let binary = std::env::var_os("FR_URING_FR_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_frankenredis")));
    let redis_binary = std::env::var_os("FR_URING_REDIS_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../legacy_redis_code/redis/src/redis-server")
        });
    assert!(
        binary.is_file(),
        "FrankenRedis executable is missing: {}",
        binary.display()
    );
    assert!(
        redis_binary.is_file(),
        "vendored Redis executable is missing: {}",
        redis_binary.display()
    );
    let harness = std::env::current_exe().expect("locate running harness ELF");
    // First benchmark-authored line: the executing harness identifies its own
    // ELF before any child process is started.
    println!(
        "HARNESS_ELF_SELF_REPORT sha256={} arms=mio_a,mio_b,io_uring,redis",
        hash_path(&harness)
    );
    let redis_version = command_output(
        redis_binary
            .to_str()
            .expect("vendored Redis executable path must be UTF-8"),
        &["--version"],
    );
    assert!(
        redis_version.status.success(),
        "vendored Redis --version must succeed"
    );
    let redis_version = String::from_utf8(redis_version.stdout).expect("Redis version is UTF-8");
    assert!(
        redis_version.contains("v=7.2.4"),
        "expected vendored Redis 7.2.4, got {redis_version:?}"
    );
    println!("INCUMBENT_VERSION {}", redis_version.trim());

    let hostname = command_output("hostname", &[]);
    assert!(hostname.status.success(), "hostname failed");
    let hostname = String::from_utf8_lossy(&hostname.stdout).trim().to_owned();
    let kernel = command_output("uname", &["-r"]);
    assert!(kernel.status.success(), "uname failed");
    let kernel = String::from_utf8_lossy(&kernel.stdout).trim().to_owned();
    let disabled = fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
        .unwrap_or_else(|_| "unknown".into());
    println!(
        "WORKER_ID host={hostname} kernel={kernel} io_uring_disabled={}",
        disabled.trim()
    );
    println!(
        "DECISION_CONTRACT same_invocation_aa=true live_redis_arm=true \
bootstrap_median_ci_gate=true cv_provenance_only=true never_cv_gate=true"
    );

    let samples = parse_usize_env("FR_URING_AB_SAMPLES", DEFAULT_SAMPLES);
    assert!(samples >= 8, "median CI requires at least eight samples");
    let client_thread_counts = parse_client_thread_counts();
    let max_client_threads = *client_thread_counts
        .last()
        .expect("client thread sweep is non-empty");
    let clients = parse_usize_env(
        "FR_URING_AB_CLIENTS",
        DEFAULT_CLIENTS.max(max_client_threads),
    );
    assert!(
        clients >= max_client_threads,
        "FR_URING_AB_CLIENTS={clients} must cover max client thread count \
{max_client_threads}"
    );
    let ops_per_sample = parse_usize_env("FR_URING_AB_OPS_PER_SAMPLE", DEFAULT_OPS_PER_SAMPLE);
    let interleave_groups =
        parse_usize_env("FR_URING_AB_INTERLEAVE_GROUPS", DEFAULT_INTERLEAVE_GROUPS);
    let profile_seconds = parse_u64_env("FR_URING_AB_PROFILE_SECONDS", DEFAULT_PROFILE_SECONDS);
    let workloads = Workload::parse_list();
    let pipelines = parse_pipelines();
    let bitpos_range_floor_ab = parse_bool_env("FR_URING_AB_BITPOS_RANGE_FLOOR");
    let bitfield_ro_two_get_floor_ab = parse_bool_env("FR_URING_AB_BITFIELD_RO_TWO_GET_FLOOR");
    let object_encoding_floor_ab = parse_bool_env("FR_URING_AB_OBJECT_ENCODING_FLOOR");
    let object_refcount_floor_ab = parse_bool_env("FR_URING_AB_OBJECT_REFCOUNT_FLOOR");
    let dbsize_floor_ab = parse_bool_env("FR_URING_AB_DBSIZE_FLOOR");
    let echo_floor_ab = parse_bool_env("FR_URING_AB_ECHO_FLOOR");
    let wait_zero_floor_ab = parse_bool_env("FR_URING_AB_WAIT_ZERO_FLOOR");
    let xtrim_minid_noop_ab = parse_bool_env("FR_URING_AB_XTRIM_MINID_NOOP");
    let xtrim_minid_noop_floor_ab = parse_bool_env("FR_URING_AB_XTRIM_MINID_NOOP_FLOOR");
    let xdel_missing_floor_ab = parse_bool_env("FR_URING_AB_XDEL_MISSING_FLOOR");
    let xack_missing_floor_ab = parse_bool_env("FR_URING_AB_XACK_MISSING_FLOOR");
    let xrange_zero_floor_ab = parse_bool_env("FR_URING_AB_XRANGE_ZERO_FLOOR");
    let xrevrange_zero_floor_ab = parse_bool_env("FR_URING_AB_XREVRANGE_ZERO_FLOOR");
    let lrange_floor_ab = parse_bool_env("FR_URING_AB_LRANGE_FLOOR");
    assert!(!workloads.is_empty(), "at least one workload is required");
    assert!(!pipelines.is_empty(), "at least one pipeline is required");
    #[cfg(not(feature = "perf-ab-bitpos-range-floor"))]
    assert!(
        !bitpos_range_floor_ab,
        "FR_URING_AB_BITPOS_RANGE_FLOOR=1 requires \
--features perf-ab-bitpos-range-floor"
    );
    #[cfg(not(feature = "perf-ab-bitfield-ro-two-get-floor"))]
    assert!(
        !bitfield_ro_two_get_floor_ab,
        "FR_URING_AB_BITFIELD_RO_TWO_GET_FLOOR=1 requires \
--features perf-ab-bitfield-ro-two-get-floor"
    );
    #[cfg(not(feature = "perf-ab-object-encoding-floor"))]
    assert!(
        !object_encoding_floor_ab,
        "FR_URING_AB_OBJECT_ENCODING_FLOOR=1 requires \
--features perf-ab-object-encoding-floor"
    );
    #[cfg(not(feature = "perf-ab-object-refcount-floor"))]
    assert!(
        !object_refcount_floor_ab,
        "FR_URING_AB_OBJECT_REFCOUNT_FLOOR=1 requires \
--features perf-ab-object-refcount-floor"
    );
    #[cfg(not(feature = "perf-ab-dbsize-floor"))]
    assert!(
        !dbsize_floor_ab,
        "FR_URING_AB_DBSIZE_FLOOR=1 requires --features perf-ab-dbsize-floor"
    );
    #[cfg(not(feature = "perf-ab-echo-floor"))]
    assert!(
        !echo_floor_ab,
        "FR_URING_AB_ECHO_FLOOR=1 requires --features perf-ab-echo-floor"
    );
    #[cfg(not(feature = "perf-ab-wait-zero-floor"))]
    assert!(
        !wait_zero_floor_ab,
        "FR_URING_AB_WAIT_ZERO_FLOOR=1 requires --features perf-ab-wait-zero-floor"
    );
    #[cfg(not(feature = "perf-ab-xtrim-minid-noop"))]
    assert!(
        !xtrim_minid_noop_ab,
        "FR_URING_AB_XTRIM_MINID_NOOP=1 requires --features perf-ab-xtrim-minid-noop"
    );
    #[cfg(not(feature = "perf-ab-xtrim-minid-noop-floor"))]
    assert!(
        !xtrim_minid_noop_floor_ab,
        "FR_URING_AB_XTRIM_MINID_NOOP_FLOOR=1 requires \
--features perf-ab-xtrim-minid-noop-floor"
    );
    #[cfg(not(feature = "perf-ab-xdel-missing-floor"))]
    assert!(
        !xdel_missing_floor_ab,
        "FR_URING_AB_XDEL_MISSING_FLOOR=1 requires --features perf-ab-xdel-missing-floor"
    );
    #[cfg(not(feature = "perf-ab-xack-missing-floor"))]
    assert!(
        !xack_missing_floor_ab,
        "FR_URING_AB_XACK_MISSING_FLOOR=1 requires --features perf-ab-xack-missing-floor"
    );
    #[cfg(not(feature = "perf-ab-xrange-zero-floor"))]
    assert!(
        !xrange_zero_floor_ab,
        "FR_URING_AB_XRANGE_ZERO_FLOOR=1 requires --features perf-ab-xrange-zero-floor"
    );
    #[cfg(not(feature = "perf-ab-xrevrange-zero-floor"))]
    assert!(
        !xrevrange_zero_floor_ab,
        "FR_URING_AB_XREVRANGE_ZERO_FLOOR=1 requires \
--features perf-ab-xrevrange-zero-floor"
    );
    #[cfg(not(feature = "perf-ab-lrange-floor"))]
    assert!(
        !lrange_floor_ab,
        "FR_URING_AB_LRANGE_FLOOR=1 requires --features perf-ab-lrange-floor"
    );
    assert!(
        usize::from(bitpos_range_floor_ab)
            + usize::from(bitfield_ro_two_get_floor_ab)
            + usize::from(object_encoding_floor_ab)
            + usize::from(object_refcount_floor_ab)
            + usize::from(dbsize_floor_ab)
            + usize::from(echo_floor_ab)
            + usize::from(wait_zero_floor_ab)
            + usize::from(xtrim_minid_noop_ab)
            + usize::from(xtrim_minid_noop_floor_ab)
            + usize::from(xdel_missing_floor_ab)
            + usize::from(xack_missing_floor_ab)
            + usize::from(xrange_zero_floor_ab)
            + usize::from(xrevrange_zero_floor_ab)
            + usize::from(lrange_floor_ab)
            <= 1,
        "run only one command-shape floor experiment per invocation"
    );
    let command_floor_ab = if bitpos_range_floor_ab {
        CommandFloorAb::BitposRange
    } else if bitfield_ro_two_get_floor_ab {
        CommandFloorAb::BitfieldRoTwoGet
    } else if object_encoding_floor_ab {
        CommandFloorAb::ObjectEncoding
    } else if object_refcount_floor_ab {
        CommandFloorAb::ObjectRefcount
    } else if dbsize_floor_ab {
        CommandFloorAb::Dbsize
    } else if echo_floor_ab {
        CommandFloorAb::Echo
    } else if wait_zero_floor_ab {
        CommandFloorAb::WaitZero
    } else if xtrim_minid_noop_ab {
        CommandFloorAb::XtrimMinidNoop
    } else if xtrim_minid_noop_floor_ab {
        CommandFloorAb::XtrimMinidNoopFloor
    } else if xdel_missing_floor_ab {
        CommandFloorAb::XdelMissingFloor
    } else if xack_missing_floor_ab {
        CommandFloorAb::XackMissingFloor
    } else if xrange_zero_floor_ab {
        CommandFloorAb::XrangeZeroFloor
    } else if xrevrange_zero_floor_ab {
        CommandFloorAb::XrevrangeZeroFloor
    } else if lrange_floor_ab {
        CommandFloorAb::LrangeFloor
    } else {
        CommandFloorAb::None
    };
    if bitpos_range_floor_ab {
        assert_eq!(
            workloads,
            [Workload::BitposRange],
            "the BITPOS range floor A/B must isolate the exact profiled workload"
        );
    }
    if bitfield_ro_two_get_floor_ab {
        assert_eq!(
            workloads,
            [Workload::BitfieldRoTwoGet],
            "the BITFIELD_RO two-GET floor A/B must isolate the exact profiled workload"
        );
    }
    if object_encoding_floor_ab {
        assert_eq!(
            workloads,
            [Workload::ObjectEncoding],
            "the OBJECT ENCODING floor A/B must isolate the exact profiled workload"
        );
    }
    if object_refcount_floor_ab {
        assert_eq!(
            workloads,
            [Workload::ObjectRefcount],
            "the OBJECT REFCOUNT floor A/B must isolate the exact profiled workload"
        );
    }
    if dbsize_floor_ab {
        assert_eq!(
            workloads,
            [Workload::Dbsize],
            "the DBSIZE floor A/B must isolate the exact profiled workload"
        );
    }
    if echo_floor_ab {
        assert_eq!(
            workloads,
            [Workload::Echo],
            "the ECHO floor A/B must isolate the exact profiled workload"
        );
    }
    if wait_zero_floor_ab {
        assert_eq!(
            workloads,
            [Workload::WaitZero],
            "the WAIT 0 0 floor A/B must isolate the exact profiled workload"
        );
    }
    if xtrim_minid_noop_ab {
        assert_eq!(
            workloads,
            [Workload::XtrimMinidNoop],
            "the XTRIM MINID no-op A/B must isolate the exact profiled workload"
        );
    }
    if xtrim_minid_noop_floor_ab {
        assert_eq!(
            workloads,
            [Workload::XtrimMinidNoop],
            "the XTRIM MINID no-op floor A/B must isolate the exact profiled workload"
        );
    }
    if xdel_missing_floor_ab {
        assert_eq!(
            workloads,
            [Workload::XdelMissing],
            "the XDEL missing-ID floor A/B must isolate the exact profiled workload"
        );
    }
    if xack_missing_floor_ab {
        assert_eq!(
            workloads,
            [Workload::XackMissing],
            "the XACK missing-ID floor A/B must isolate the exact profiled workload"
        );
    }
    if xrange_zero_floor_ab {
        assert_eq!(
            workloads,
            [Workload::XrangeZero],
            "the XRANGE zero interval floor A/B must isolate the exact profiled workload"
        );
    }
    if xrevrange_zero_floor_ab {
        assert_eq!(
            workloads,
            [Workload::XrevrangeZero],
            "the XREVRANGE zero interval floor A/B must isolate the exact profiled workload"
        );
    }
    if lrange_floor_ab {
        assert_eq!(
            workloads,
            [Workload::LrangeInverted],
            "the LRANGE floor A/B must isolate the exact profiled workload"
        );
    }

    let perf_version = command_output("perf", &["--version"]);
    assert!(
        perf_version.status.success(),
        "worker must provide perf for profile attribution"
    );
    if client_thread_counts.len() > 1 {
        assert_eq!(
            hostname, "threadripperje",
            "thread-scaling sweeps must execute on trj/threadripperje"
        );
    }
    let (physical_cores, logical_threads) = machine_topology();
    let ram_kib = machine_ram_kib();
    let numa_nodes = machine_numa_nodes();
    let (client_cpu_order, server_core, process_cpuset_cap) =
        choose_client_cpu_order(max_client_threads);
    assert_eq!(
        process_cpuset_cap.len(),
        logical_threads,
        "host-wide benchmarking requires access to every logical CPU; \
process cpuset has {} of {logical_threads}",
        process_cpuset_cap.len()
    );
    let initial_cpu_frequency_policy = cpu_frequency_policy(&process_cpuset_cap);
    print_cpu_frequency_policy(
        "initial_pre_measurement",
        &process_cpuset_cap,
        &initial_cpu_frequency_policy,
    );
    assert_host_wide_quiescence(&process_cpuset_cap, "initial_pre_pin");
    let initial_client_threads = client_thread_counts[0];
    let initial_client_affinity = pin_client_process(&client_cpu_order, initial_client_threads);
    println!(
        "SCALING_HARDWARE_PROVENANCE host_identity={hostname} \
physical_cores={physical_cores} logical_threads={logical_threads} \
ram_kib={ram_kib} numa_nodes={numa_nodes} \
thread_counts_requested={client_thread_counts:?} \
runtime_detected_isa={} process_cpuset_cap={process_cpuset_cap:?} \
initial_client_affinity={initial_client_affinity:?} \
server_affinity_cpu={server_core}",
        runtime_isa_features()
    );
    println!(
        "THREAD_COUNT_SEMANTICS candidate_command_execution_threads=1 \
control_command_execution_threads=1 incumbent_command_execution_threads=1 \
client_driver_threads_are_saturation_axis=true \
campaign_multicore_scaling_result=false"
    );
    let root = unique_root();
    println!("ARTIFACT_ROOT {}", root.display());

    let initial_client_shape = ClientShape {
        connections: clients,
        driver_threads: initial_client_threads,
    };
    let mut servers = [
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::MioA,
            &root,
            server_core,
            initial_client_shape,
            command_floor_ab,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::MioB,
            &root,
            server_core,
            initial_client_shape,
            command_floor_ab,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::IoUring,
            &root,
            server_core,
            initial_client_shape,
            command_floor_ab,
        ),
        Server::spawn(
            &binary,
            &redis_binary,
            Arm::Redis,
            &root,
            server_core,
            initial_client_shape,
            command_floor_ab,
        ),
    ];
    let server_hashes = Arm::ALL.map(|arm| servers[arm.index()].executing_elf_sha256());
    assert_eq!(
        server_hashes[Arm::MioA.index()],
        server_hashes[Arm::MioB.index()],
        "A/A controls must execute the same ELF"
    );
    assert_eq!(
        server_hashes[Arm::MioA.index()],
        server_hashes[Arm::IoUring.index()],
        "control and candidate must execute the same FrankenRedis ELF"
    );
    for arm in Arm::ALL {
        println!(
            "SERVER_ELF_SELF_REPORT arm={} pid={} sha256={} \
process_threads_observed={} command_execution_threads=1 affinity_cpus={:?}",
            arm.name(),
            servers[arm.index()].child.id(),
            server_hashes[arm.index()],
            servers[arm.index()].observed_thread_count(),
            servers[arm.index()].affinity_cpus()
        );
    }
    if bitpos_range_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_bitpos_range_floor \
control_b=io_uring+frozen_pre_bitpos_range_floor \
candidate=io_uring+bitpos_range_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(BITPOS_RANGE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(BITPOS_RANGE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(BITPOS_RANGE_FLOOR_CONTROL_ENV, None);
    } else if bitfield_ro_two_get_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_bitfield_ro_two_get_floor \
control_b=io_uring+frozen_pre_bitfield_ro_two_get_floor \
candidate=io_uring+bitfield_ro_two_get_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(BITFIELD_RO_TWO_GET_FLOOR_CONTROL_ENV, None);
    } else if object_encoding_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_object_encoding_floor \
control_b=io_uring+frozen_pre_object_encoding_floor \
candidate=io_uring+object_encoding_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(OBJECT_ENCODING_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(OBJECT_ENCODING_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(OBJECT_ENCODING_FLOOR_CONTROL_ENV, None);
    } else if object_refcount_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_object_refcount_floor \
control_b=io_uring+frozen_pre_object_refcount_floor \
candidate=io_uring+object_refcount_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(OBJECT_REFCOUNT_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(OBJECT_REFCOUNT_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(OBJECT_REFCOUNT_FLOOR_CONTROL_ENV, None);
    } else if dbsize_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_dbsize_floor \
control_b=io_uring+frozen_pre_dbsize_floor \
candidate=io_uring+dbsize_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()].assert_environment_value(DBSIZE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()].assert_environment_value(DBSIZE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(DBSIZE_FLOOR_CONTROL_ENV, None);
    } else if echo_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_echo_floor \
control_b=io_uring+frozen_pre_echo_floor \
candidate=io_uring+echo_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()].assert_environment_value(ECHO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()].assert_environment_value(ECHO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(ECHO_FLOOR_CONTROL_ENV, None);
    } else if wait_zero_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_wait_zero_floor \
control_b=io_uring+frozen_pre_wait_zero_floor \
candidate=io_uring+wait_zero_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()].assert_environment_value(WAIT_ZERO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()].assert_environment_value(WAIT_ZERO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(WAIT_ZERO_FLOOR_CONTROL_ENV, None);
    } else if xtrim_minid_noop_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+frozen_pre_xtrim_minid_noop_guard \
control_b=io_uring+frozen_pre_xtrim_minid_noop_guard \
candidate=io_uring+xtrim_minid_noop_guard incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(XTRIM_MINID_NOOP_CONTROL_ENV, None);
    } else if xtrim_minid_noop_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+guarded_generic_xtrim_minid_noop \
control_b=io_uring+guarded_generic_xtrim_minid_noop \
candidate=io_uring+xtrim_minid_noop_dispatch_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(XTRIM_MINID_NOOP_FLOOR_CONTROL_ENV, None);
    } else if xdel_missing_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+generic_xdel_missing \
control_b=io_uring+generic_xdel_missing \
candidate=io_uring+xdel_missing_dispatch_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(XDEL_MISSING_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(XDEL_MISSING_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(XDEL_MISSING_FLOOR_CONTROL_ENV, None);
    } else if xack_missing_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+generic_xack_missing \
control_b=io_uring+generic_xack_missing \
candidate=io_uring+xack_missing_dispatch_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(XACK_MISSING_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(XACK_MISSING_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(XACK_MISSING_FLOOR_CONTROL_ENV, None);
    } else if xrange_zero_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+generic_xrange_zero \
control_b=io_uring+generic_xrange_zero \
candidate=io_uring+xrange_zero_dispatch_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(XRANGE_ZERO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(XRANGE_ZERO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(XRANGE_ZERO_FLOOR_CONTROL_ENV, None);
    } else if xrevrange_zero_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+generic_xrevrange_zero \
control_b=io_uring+generic_xrevrange_zero \
candidate=io_uring+xrevrange_zero_dispatch_floor incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()]
            .assert_environment_value(XREVRANGE_ZERO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()]
            .assert_environment_value(XREVRANGE_ZERO_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()]
            .assert_environment_value(XREVRANGE_ZERO_FLOOR_CONTROL_ENV, None);
    } else if lrange_floor_ab {
        println!(
            "ARM_SEMANTICS control_a=io_uring+early_cascade_lrange \
control_b=io_uring+early_cascade_lrange \
candidate=io_uring+lrange_front_dispatch incumbent=vendored_redis_7.2.4"
        );
        for arm in [Arm::MioA, Arm::MioB, Arm::IoUring] {
            servers[arm.index()].assert_flag_reached_process();
        }
        servers[Arm::MioA.index()].assert_environment_value(LRANGE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::MioB.index()].assert_environment_value(LRANGE_FLOOR_CONTROL_ENV, Some("1"));
        servers[Arm::IoUring.index()].assert_environment_value(LRANGE_FLOOR_CONTROL_ENV, None);
    } else {
        println!(
            "ARM_SEMANTICS control_a=mio control_b=mio \
candidate=io_uring incumbent=vendored_redis_7.2.4"
        );
        servers[Arm::IoUring.index()].assert_flag_reached_process();
    }

    let mut verdicts = Vec::new();
    for &client_threads in &client_thread_counts {
        let client_shape = ClientShape {
            connections: clients,
            driver_threads: client_threads,
        };
        let client_affinity = pin_client_process(&client_cpu_order, client_threads);
        if client_threads != initial_client_threads {
            for arm in Arm::ALL {
                servers[arm.index()].replace_clients(client_shape);
            }
        }
        assert!(
            Arm::ALL
                .iter()
                .all(|arm| servers[arm.index()].client_thread_count() == client_threads),
            "every arm must use exactly {client_threads} client driver threads"
        );
        println!(
            "SCALING_POINT_PROVENANCE host_identity={hostname} \
physical_cores={physical_cores} logical_threads={logical_threads} \
ram_kib={ram_kib} numa_nodes={numa_nodes} \
thread_count_actually_used={client_threads} \
client_driver_threads_actual={client_threads} client_connections={clients} \
candidate_command_execution_threads=1 control_command_execution_threads=1 \
incumbent_command_execution_threads=1 \
candidate_process_threads_observed={} incumbent_process_threads_observed={} \
runtime_detected_isa={} process_cpuset_cap={process_cpuset_cap:?} \
client_affinity={client_affinity:?} server_affinity={:?}",
            servers[Arm::IoUring.index()].observed_thread_count(),
            servers[Arm::Redis.index()].observed_thread_count(),
            runtime_isa_features(),
            servers[Arm::IoUring.index()].affinity_cpus()
        );
        for &workload in &workloads {
            for pipeline in &pipelines {
                // Read-only/profiled workloads may require seeded server state.
                // Prime every arm before profiling, then measure_configuration
                // re-primes all four arms immediately before its warmup.
                prefill(&mut servers, workload);
                profile_io_uring_path(
                    &mut servers[Arm::IoUring.index()],
                    &root,
                    profile_seconds,
                    workload,
                    *pipeline,
                    client_threads,
                );
                let measured = measure_configuration(
                    &mut servers,
                    workload,
                    *pipeline,
                    MeasurementConfig {
                        client_shape,
                        samples,
                        ops_per_sample,
                        interleave_groups,
                    },
                    &process_cpuset_cap,
                );
                verdicts.push((
                    client_threads,
                    workload.name(),
                    *pipeline,
                    adjudicate(workload, *pipeline, client_threads, &measured),
                ));
            }
        }
    }

    let final_loads = observed_core_loads();
    println!("FINAL_CORE_LOAD_SNAPSHOT {final_loads:?}");
    let final_cpu_frequency_policy = cpu_frequency_policy(&process_cpuset_cap);
    print_cpu_frequency_policy(
        "final_post_measurement",
        &process_cpuset_cap,
        &final_cpu_frequency_policy,
    );
    assert_eq!(
        initial_cpu_frequency_policy, final_cpu_frequency_policy,
        "CPU frequency policy changed during the benchmark invocation"
    );
    println!("VERDICT_MATRIX {verdicts:?}");
}
