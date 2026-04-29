#!/usr/bin/env python3
"""Generate structured corpus seeds for fuzz_eventloop_validators.

The fuzz target runs every input through `fuzz_raw_phase_trace`,
converting each byte into an `EventLoopPhase` via `byte % 5`:

    0 → BeforeSleep
    1 → Poll
    2 → FileDispatch
    3 → TimeDispatch
    4 → AfterSleep

Then asserts `replay_phase_trace(trace) ==
model_replay_phase_trace(trace)`. The bytes also drive the
structured-arbitrary path; arbitrary's format isn't version-stable
so we only seed mode-0 here.

Each seed targets a meaningful phase ordering shape:

  - empty trace (no phases)
  - single-phase traces (each phase once)
  - canonical loop iteration (BS, Poll, FD, TD, AS)
  - two full iterations
  - just BeforeSleep + Poll (truncated iteration)
  - Poll → AfterSleep (skip FD/TD — invalid ordering)
  - FileDispatch → Poll (out-of-order — invalid)
  - long trace at MAX_TRACE_LEN boundary (64 bytes)
  - all-same-phase repetitions
  - dense alternation
  - ASCII text whose byte mod-5 hits a useful sequence
  - high-byte-value bytes (255 % 5 = 0 → BeforeSleep)

Run:
    python3 fuzz/scripts/gen_eventloop_validators_seeds.py
"""
from __future__ import annotations

from pathlib import Path


def seed(label: str, body: bytes) -> tuple[str, bytes]:
    return (label, body)


def main() -> None:
    repo = Path(__file__).resolve().parent.parent.parent
    out_dir = repo / "fuzz" / "corpus" / "fuzz_eventloop_validators"
    out_dir.mkdir(parents=True, exist_ok=True)

    # Phase byte abbreviations (each maps to mod-5 == that phase).
    BS, POLL, FD, TD, AS = 0, 1, 2, 3, 4

    seeds: list[tuple[str, bytes]] = [
        seed("empty_trace.bin", b""),
        seed("single_before_sleep.bin", bytes([BS])),
        seed("single_poll.bin", bytes([POLL])),
        seed("single_file_dispatch.bin", bytes([FD])),
        seed("single_time_dispatch.bin", bytes([TD])),
        seed("single_after_sleep.bin", bytes([AS])),
        seed("canonical_loop_iteration.bin", bytes([BS, POLL, FD, TD, AS])),
        seed("two_full_iterations.bin",
             bytes([BS, POLL, FD, TD, AS] * 2)),
        seed("three_full_iterations.bin",
             bytes([BS, POLL, FD, TD, AS] * 3)),
        seed("truncated_iteration_bs_then_poll.bin", bytes([BS, POLL])),
        seed("skip_dispatches_poll_to_after_sleep.bin",
             bytes([BS, POLL, AS])),
        seed("file_then_poll_out_of_order.bin",
             bytes([BS, FD, POLL, TD, AS])),
        seed("after_sleep_first.bin",
             bytes([AS, BS, POLL, FD, TD, AS])),
        seed("only_polls.bin", bytes([POLL] * 5)),
        seed("only_after_sleeps.bin", bytes([AS] * 5)),
        seed("only_file_dispatches.bin", bytes([FD] * 5)),
        seed("alternating_bs_as.bin",
             bytes([BS, AS, BS, AS, BS, AS])),
        seed("alternating_poll_fd.bin",
             bytes([POLL, FD, POLL, FD, POLL, FD])),
        seed("max_trace_len_canonical.bin",
             bytes([BS, POLL, FD, TD, AS] * 12 + [BS, POLL, FD, TD])),
        # 64 bytes hits MAX_TRACE_LEN exactly.
        seed("max_trace_len_64_bytes.bin",
             bytes([BS, POLL, FD, TD, AS] * 12 + [BS, POLL, FD, TD])),
        # 65 bytes — truncated to 64 by harness.
        seed("over_max_trace_len.bin",
             bytes([BS, POLL, FD, TD, AS] * 13)),
        # ASCII text whose byte values mod 5 produce a sequence.
        # 'a'=97 → 97%5=2 (FD); 'b'=98 → 3 (TD); 'c'=99 → 4 (AS);
        # 'd'=100 → 0 (BS); 'e'=101 → 1 (Poll). So "abcde" cycles
        # through FD, TD, AS, BS, Poll.
        seed("ascii_abcde_mod5_cycle.bin", b"abcde"),
        seed("ascii_aaaaa_only_fd.bin", b"aaaaa"),
        seed("ascii_redis_word.bin", b"redis"),
        # High-value bytes: 255%5=0 (BS), 254%5=4 (AS), 253%5=3 (TD).
        seed("high_byte_values.bin", bytes([255, 254, 253, 252, 251])),
        # Mixed full + partial iterations.
        seed("two_then_partial.bin",
             bytes([BS, POLL, FD, TD, AS, BS, POLL, FD, TD, AS, BS, POLL])),
        # Multi-cycle with extra BeforeSleep at the start.
        seed("double_before_sleep_then_loop.bin",
             bytes([BS, BS, POLL, FD, TD, AS])),
        # NUL bytes alone (NUL = 0 = BeforeSleep).
        seed("nul_bytes_only.bin", b"\x00\x00\x00\x00"),
        # CRLF bytes: 0x0D=13%5=3 (TD), 0x0A=10%5=0 (BS).
        seed("crlf_bytes.bin", b"\r\n"),
        # Sparse iteration: BS without any other phase.
        seed("only_one_bs.bin", bytes([BS])),
        # Reversal: AS first, then BS-cycle.
        seed("reverse_then_canonical.bin",
             bytes([AS, TD, FD, POLL, BS])),
    ]

    for label, payload in seeds:
        path = out_dir / label
        path.write_bytes(payload)
        print(f"wrote {len(payload):4d} bytes to {path.relative_to(repo)}")
    print(f"\ngenerated {len(seeds)} corpus seeds")


if __name__ == "__main__":
    main()
