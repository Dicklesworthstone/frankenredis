//! Batched socket submission via `io_uring`, isolated behind a safe API.
//!
//! # Why this crate exists
//!
//! `fr-server` declares `#![forbid(unsafe_code)]`, and `forbid` cannot be relaxed
//! per-module. The `io_uring` submission-queue push is inherently unsafe (the
//! kernel reads the caller's buffers asynchronously), so per `AGENTS.md` — *"if
//! narrow unsafe usage is unavoidable, isolate it behind audited interfaces and
//! tests"* — the entire unsafe surface lives here, in one function, with the
//! soundness argument written out and covered by tests.
//!
//! # Measured premise and result
//!
//! A `-P1 -c50` census of both engines (2026-07-25, `perf trace -s`, 4 s window)
//! showed the write submission path is one syscall per operation for BOTH
//! FrankenRedis and vendored Redis 7.2.4, and that it dominates wall time:
//!
//! | | fr | redis 7.2.4 |
//! |---|---|---|
//! | `sendto`/`write` | 229,740 calls, **1,898 ms (47.4% of wall)** | 239,461 calls, **1,791 ms (44.8%)** |
//! | `recvfrom`/`read` | 229,700 calls, 673 ms | 238,966 calls, 776 ms |
//! | `epoll_wait` | 4,725 calls (~48 fds ready per wakeup) | 6,598 calls |
//!
//! The read side is already batched — one `epoll_wait` reports ~48 ready
//! connections — but each of those connections then pays its own `sendto`. This
//! crate groups those submissions behind one SQ publication. The counted
//! experiment reduced submissions from 1.000/op to 0.176/op, but its synchronous
//! completion wait made P1 SET wall time 8.78% worse under the repository's
//! same-ELF, same-invocation A/A+A/B median-CI gate. The implementation remains
//! default-off as a reproducible substrate for a future asynchronous-CQ design;
//! see the 2026-07-26 entry in `docs/NEGATIVE_EVIDENCE.md`.
//!
//! At `pipeline=16` there is nothing here to win — that census measures 0.13-0.16
//! syscalls per operation for both engines, because a whole pipelined batch
//! already coalesces into one write. See the 2026-07-25 entry in
//! `docs/NEGATIVE_EVIDENCE.md`. This crate targets the unpipelined regime only.

use std::io;
use std::os::fd::RawFd;

use io_uring::{IoUring, opcode, types};

/// Default submission/completion queue depth. Sized to comfortably exceed the
/// ~48 ready connections a single `epoll_wait` reports under `-c50`, so the
/// common batch never has to be chunked.
pub const DEFAULT_RING_ENTRIES: u32 = 256;

/// The outcome of one submitted send, in the same order as the request slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The kernel accepted `n` bytes. May be short — callers must treat this
    /// exactly as they treat a short `write(2)` and resubmit the remainder.
    Wrote(usize),
    /// The socket was not writable (`EAGAIN`/`EWOULDBLOCK`). Equivalent to
    /// `ErrorKind::WouldBlock` from the blocking path.
    WouldBlock,
    /// A real error; the value is a positive errno.
    Failed(i32),
}

impl SendOutcome {
    fn from_cqe_result(res: i32) -> Self {
        if res >= 0 {
            return Self::Wrote(res as usize);
        }
        let errno = -res;
        if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
            Self::WouldBlock
        } else {
            Self::Failed(errno)
        }
    }
}

/// One pending socket write.
#[derive(Debug, Clone, Copy)]
pub struct SendJob<'a> {
    pub fd: RawFd,
    pub bytes: &'a [u8],
}

/// A reusable `io_uring` instance for batched socket sends.
///
/// Not `Sync`; intended to be owned by the single event-loop thread, mirroring
/// how `mio::Poll` is owned today.
pub struct BatchWriter {
    ring: IoUring,
    entries: usize,
}

impl BatchWriter {
    /// Create a ring with `entries` submission slots.
    ///
    /// Returns `Err` when the kernel lacks `io_uring` or it is administratively
    /// disabled (`/proc/sys/kernel/io_uring_disabled`). `fr-server` treats that
    /// as a fatal startup error only when the operator explicitly requested the
    /// runtime flag; without the flag, it never constructs a ring.
    pub fn new(entries: u32) -> io::Result<Self> {
        let ring = IoUring::new(entries)?;
        Ok(Self {
            ring,
            entries: entries as usize,
        })
    }

    /// Submit every job as an `IORING_OP_SEND`, wait for all of them, and append
    /// one [`SendOutcome`] per job to `out` **in the same order as `jobs`**.
    ///
    /// `out` is cleared first. On success `out.len() == jobs.len()`.
    ///
    /// # Soundness
    ///
    /// The unsafe obligation on `SubmissionQueue::push_multiple` is that each
    /// SQE's buffer stays valid, in place, and unaliased until the kernel
    /// completes that operation. This function upholds it structurally:
    ///
    /// 1. Every buffer pointer comes from `jobs[i].bytes`, borrowed for the whole
    ///    call — the borrow checker guarantees the caller cannot move, free, or
    ///    mutate them while `send_batch` runs.
    /// 2. Each chunk is submitted with `submit_and_wait(n)` and then drained of
    ///    exactly `n` completions before the next chunk is pushed and before the
    ///    function returns. No SQE is ever in flight past the end of the call, so
    ///    no pointer outlives the borrow.
    /// 3. The ring is `&mut self`, so no other thread can submit concurrently.
    /// 4. Sends are read-only for the kernel, so the buffers are never aliased
    ///    mutably.
    ///
    /// The one thing this deliberately does *not* do is keep operations in flight
    /// across calls. That would be faster still, but it moves buffer lifetime out
    /// of the type system and into a manual registry — a use-after-free in the
    /// reply path would corrupt client data. The synchronous form proves that
    /// submissions can be collapsed, but the 2026-07-26 wall gate rejected its
    /// forced completion wait; it is not a production performance win.
    #[inline(never)]
    pub fn send_batch(
        &mut self,
        jobs: &[SendJob<'_>],
        out: &mut Vec<SendOutcome>,
    ) -> io::Result<()> {
        out.clear();
        if jobs.is_empty() {
            return Ok(());
        }
        // Reserve so the completion loop below never reallocates mid-drain.
        out.reserve(jobs.len());

        for chunk in jobs.chunks(self.entries) {
            self.submit_chunk(chunk, out)?;
        }
        Ok(())
    }

    fn submit_chunk(
        &mut self,
        chunk: &[SendJob<'_>],
        out: &mut Vec<SendOutcome>,
    ) -> io::Result<()> {
        let base = out.len();
        let entries: Vec<_> = chunk
            .iter()
            .enumerate()
            .map(|(index, job)| {
                opcode::Send::new(
                    types::Fd(job.fd),
                    job.bytes.as_ptr(),
                    // A single send is capped at u32::MAX bytes; longer buffers
                    // are handled by the caller's short-write loop.
                    job.bytes.len().min(u32::MAX as usize) as u32,
                )
                // Without MSG_DONTWAIT, io_uring internally polls a full socket.
                // The event loop must regain control and arm EPOLLOUT instead,
                // exactly like the existing mio path.
                .flags(libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL)
                .build()
                .user_data(index as u64)
            })
            .collect();
        {
            let mut sq = self.ring.submission();
            // SAFETY: see the soundness argument on `send_batch`. `push_multiple`
            // is all-or-nothing, so its error path cannot publish a prefix whose
            // borrowed buffer pointers would outlive this call. On success every
            // SQE is completed and drained below before `submit_chunk` returns.
            unsafe {
                if sq.push_multiple(&entries).is_err() {
                    return Err(io::Error::other("io_uring submission queue full"));
                }
            }
            sq.sync();
        }

        let want = chunk.len();
        out.resize(base + want, SendOutcome::WouldBlock);

        // Once the SQ tail is published, returning before every CQE is observed
        // would let the kernel retain pointers into the caller's borrowed reply
        // buffers. EINTR is therefore retried and all available completions are
        // drained on every pass. A non-recoverable io_uring_enter failure after
        // publication cannot be exposed as a safe Rust error; aborting the
        // explicitly opted-in process is the only sound fail-closed outcome.
        let mut completed = 0usize;
        let mut malformed = false;
        let mut seen = vec![false; want];
        while completed < want {
            match self.ring.submit_and_wait(want - completed) {
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    eprintln!(
                        "fatal: io_uring_enter failed after publishing borrowed send buffers: {err}"
                    );
                    std::process::abort();
                }
            }

            // Completions arrive in arbitrary order; `user_data` carries the
            // index within this chunk, so results are restored to request order.
            for cqe in self.ring.completion() {
                let index = cqe.user_data() as usize;
                if index >= want || seen[index] {
                    malformed = true;
                } else {
                    seen[index] = true;
                    out[base + index] = SendOutcome::from_cqe_result(cqe.result());
                }
                completed += 1;
                if completed == want {
                    break;
                }
            }
        }

        if malformed || seen.iter().any(|entry| !entry) {
            // Some sends may already have reached the peer. Returning an error
            // would leave the caller's cursors unchanged and a retry could
            // duplicate bytes, so this cannot be exposed as recoverable.
            eprintln!("fatal: io_uring returned malformed completion identifiers");
            std::process::abort();
        }
        Ok(())
    }
}

/// Probe whether a usable ring can be created on this kernel.
///
/// Tests use this to distinguish real ring coverage from a green no-op on a
/// kernel where `io_uring` is unavailable.
#[must_use]
pub fn is_available() -> bool {
    IoUring::new(8).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// Every other test in this module early-returns when `io_uring` is
    /// unavailable, which would make the whole suite a green no-op on a host or
    /// CI worker without it — the exact "the bench never executed the code under
    /// test" failure this project's ledger exists to catch. This test makes that
    /// state impossible to mistake for a pass: if it is RED, treat the other
    /// results in this module as vacuous.
    #[test]
    fn io_uring_must_be_available_or_the_rest_of_this_suite_is_vacuous() {
        assert!(
            is_available(),
            "io_uring is unavailable on this host, so every other test in this \
             module silently skipped. Check /proc/sys/kernel/io_uring_disabled \
             (0 = enabled) and the kernel version; do NOT read the green results \
             above as coverage."
        );
    }

    #[test]
    fn batched_sends_deliver_every_payload_intact_and_in_request_order() {
        if !is_available() {
            eprintln!("io_uring unavailable; skipping");
            return;
        }
        let mut writer = BatchWriter::new(DEFAULT_RING_ENTRIES).expect("ring");

        // Four independent socket pairs, each expecting a distinct payload —
        // this is the property that matters for the reply path: batching must
        // not cross-deliver or reorder bytes between connections.
        let payloads: [&[u8]; 4] = [b"+OK\r\n", b"$5\r\nhello\r\n", b":42\r\n", b"*0\r\n"];
        let pairs: Vec<(UnixStream, UnixStream)> = (0..4)
            .map(|_| UnixStream::pair().expect("socketpair"))
            .collect();

        let jobs: Vec<SendJob<'_>> = pairs
            .iter()
            .zip(payloads.iter())
            .map(|((tx, _rx), bytes)| SendJob {
                fd: tx.as_raw_fd(),
                bytes,
            })
            .collect();

        let mut out = Vec::new();
        writer.send_batch(&jobs, &mut out).expect("send_batch");
        assert_eq!(out.len(), jobs.len());

        for (index, outcome) in out.iter().enumerate() {
            assert_eq!(
                *outcome,
                SendOutcome::Wrote(payloads[index].len()),
                "job {index} outcome"
            );
        }

        for (index, (_tx, rx)) in pairs.iter().enumerate() {
            let mut buf = vec![0u8; payloads[index].len()];
            let mut rx = rx;
            rx.read_exact(&mut buf).expect("read back");
            assert_eq!(buf, payloads[index], "payload {index} must arrive intact");
        }
    }

    #[test]
    fn empty_batch_is_a_no_op() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let mut out = vec![SendOutcome::Wrote(7)];
        writer.send_batch(&[], &mut out).expect("empty batch");
        assert!(
            out.is_empty(),
            "out must be cleared even for an empty batch"
        );
    }

    #[test]
    fn batch_larger_than_the_ring_is_chunked_and_still_ordered() {
        if !is_available() {
            return;
        }
        // Ring smaller than the batch forces the chunking path.
        let mut writer = BatchWriter::new(8).expect("ring");
        let pairs: Vec<(UnixStream, UnixStream)> = (0..20)
            .map(|_| UnixStream::pair().expect("socketpair"))
            .collect();
        let bodies: Vec<Vec<u8>> = (0..20u8)
            .map(|i| vec![b'a' + (i % 26); 1 + i as usize])
            .collect();
        let jobs: Vec<SendJob<'_>> = pairs
            .iter()
            .zip(bodies.iter())
            .map(|((tx, _rx), body)| SendJob {
                fd: tx.as_raw_fd(),
                bytes: body.as_slice(),
            })
            .collect();

        let mut out = Vec::new();
        writer.send_batch(&jobs, &mut out).expect("send_batch");
        assert_eq!(out.len(), 20);
        for (index, outcome) in out.iter().enumerate() {
            assert_eq!(
                *outcome,
                SendOutcome::Wrote(bodies[index].len()),
                "job {index}"
            );
        }
        for (index, (_tx, rx)) in pairs.iter().enumerate() {
            let mut buf = vec![0u8; bodies[index].len()];
            let mut rx = rx;
            rx.read_exact(&mut buf).expect("read back");
            assert_eq!(buf, bodies[index], "payload {index}");
        }
    }

    #[test]
    fn closed_peer_surfaces_as_failed_not_as_a_silent_success() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let (tx, rx) = UnixStream::pair().expect("socketpair");
        drop(rx);
        let jobs = [SendJob {
            fd: tx.as_raw_fd(),
            bytes: b"payload",
        }];
        let mut out = Vec::new();
        writer.send_batch(&jobs, &mut out).expect("send_batch");
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0], SendOutcome::Failed(_)),
            "a closed peer must surface as Failed, got {:?}",
            out[0]
        );
    }

    #[test]
    fn saturated_blocking_socket_returns_would_block_without_internal_polling() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let (mut tx, mut rx) = UnixStream::pair().expect("socketpair");

        // Fill the send buffer in nonblocking mode, then restore blocking mode.
        // MSG_DONTWAIT on the SQE—not O_NONBLOCK on the file description—must
        // make the batched send return control to the event loop promptly.
        tx.set_nonblocking(true).expect("set nonblocking");
        let fill = [0xa5; 64 * 1024];
        loop {
            match tx.write(&fill) {
                Ok(0) => panic!("socket accepted a zero-length write"),
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("failed while saturating socket: {err}"),
            }
        }
        tx.set_nonblocking(false).expect("restore blocking mode");

        // If the SQE accidentally omits MSG_DONTWAIT, release pressure after one
        // second so the test fails on elapsed time instead of hanging forever.
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let rescue = thread::spawn(move || {
            if cancel_rx.recv_timeout(Duration::from_secs(1)).is_err() {
                let mut drain = [0u8; 64 * 1024];
                let _ = rx.read(&mut drain);
            }
        });

        let jobs = [SendJob {
            fd: tx.as_raw_fd(),
            bytes: b"x",
        }];
        let mut out = Vec::new();
        let started = Instant::now();
        writer.send_batch(&jobs, &mut out).expect("send_batch");
        let elapsed = started.elapsed();
        let _ = cancel_tx.send(());
        rescue.join().expect("rescue thread");

        assert_eq!(out, [SendOutcome::WouldBlock]);
        assert!(
            elapsed < Duration::from_millis(250),
            "saturated send internally polled for {elapsed:?}; expected prompt WouldBlock"
        );
    }
}
