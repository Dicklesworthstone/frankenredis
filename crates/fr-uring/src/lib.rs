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
//! Readiness discovery is already batched — one `epoll_wait` reports ~48 ready
//! connections — but the server historically paid one `read` and one `sendto`
//! syscall for every ready connection. This crate groups both directions behind
//! SQ publication. Its owned-buffer APIs keep allocations in fixed registries
//! across event-loop iterations so submission does not wait for completions. The
//! feature and runtime flag remain default-off while this implementation is
//! evaluated.
//!
//! At `pipeline=16`, send batching alone has little to win — a whole pipelined
//! batch already coalesces into one write. Receive batching still removes the
//! per-ready-connection syscall when many connections wake together, while the
//! unpipelined regime can benefit in both directions.

use std::io;
use std::os::fd::RawFd;

use io_uring::{IoUring, opcode, squeue, types};

/// The crate's single unsafe operation. Both callers establish that every SQE
/// buffer remains valid through its CQE before reaching this helper.
fn push_entries(
    sq: &mut squeue::SubmissionQueue<'_>,
    entries: &[squeue::Entry],
) -> Result<(), squeue::PushError> {
    // SAFETY: this private helper is called only from the owned send/receive
    // submitters, whose fixed registries own every buffer through completion,
    // and `submit_chunk`, whose synchronous drain keeps every borrow alive
    // through completion.
    unsafe { sq.push_multiple(entries) }
}

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

/// One socket write whose buffer ownership moves into [`BatchWriter`] until the
/// kernel posts its completion.
#[derive(Debug)]
pub struct OwnedSendJob {
    /// Caller-defined identity returned unchanged with the completion.
    pub tag: u64,
    pub fd: RawFd,
    pub bytes: Vec<u8>,
    /// First unsent byte in `bytes`.
    pub start: usize,
}

/// One completed owned send.
#[derive(Debug)]
pub struct OwnedSendCompletion {
    pub tag: u64,
    pub bytes: Vec<u8>,
    pub start: usize,
    pub outcome: SendOutcome,
}

/// The outcome of one submitted receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvOutcome {
    /// The kernel placed `n` bytes at the front of the returned buffer.
    Read(usize),
    /// The peer performed an orderly shutdown.
    Closed,
    /// Readiness was consumed before the SQE ran (`EAGAIN`/`EWOULDBLOCK`).
    WouldBlock,
    /// The operation was interrupted before it read any bytes.
    Interrupted,
    /// A real error; the value is a positive errno.
    Failed(i32),
}

impl RecvOutcome {
    fn from_cqe_result(res: i32) -> Self {
        if res > 0 {
            return Self::Read(res as usize);
        }
        if res == 0 {
            return Self::Closed;
        }
        let errno = -res;
        if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
            Self::WouldBlock
        } else if errno == libc::EINTR {
            Self::Interrupted
        } else {
            Self::Failed(errno)
        }
    }
}

/// One socket receive whose initialized buffer ownership moves into
/// [`BatchReader`] until the kernel posts its completion.
#[derive(Debug)]
pub struct OwnedRecvJob {
    /// Caller-defined identity returned unchanged with the completion.
    pub tag: u64,
    pub fd: RawFd,
    /// Initialized writable storage. Its length is the maximum receive size.
    pub buffer: Vec<u8>,
}

/// One completed owned receive.
#[derive(Debug)]
pub struct OwnedRecvCompletion {
    pub tag: u64,
    pub buffer: Vec<u8>,
    pub outcome: RecvOutcome,
}

/// A reusable `io_uring` instance for batched socket sends.
///
/// Not `Sync`; intended to be owned by the single event-loop thread, mirroring
/// how `mio::Poll` is owned today.
pub struct BatchWriter {
    ring: IoUring,
    entries: usize,
    // Fixed after construction. Moving a `Vec<u8>` value between these slots
    // does not move its heap allocation, and no slot is taken until its CQE.
    owned_slots: Vec<Option<OwnedSendJob>>,
    free_owned_slots: Vec<usize>,
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
        let entries = entries as usize;
        Ok(Self {
            ring,
            entries,
            owned_slots: (0..entries).map(|_| None).collect(),
            free_owned_slots: (0..entries).rev().collect(),
        })
    }

    /// Number of owned sends that can be published without waiting for a CQE.
    #[must_use]
    pub fn available_owned_slots(&self) -> usize {
        self.free_owned_slots.len()
    }

    /// Whether at least one owned send is waiting for a CQE.
    #[must_use]
    pub fn has_owned_in_flight(&self) -> bool {
        self.free_owned_slots.len() != self.entries
    }

    /// Publish owned sends without waiting for their completions.
    ///
    /// On success `jobs` is empty and every buffer lives in this writer until
    /// [`Self::drain_owned`] returns it. On any recoverable validation or queue
    /// error no SQE was published and `jobs` is unchanged.
    ///
    /// # Soundness
    ///
    /// `SubmissionQueue::push_multiple` requires every buffer address to stay
    /// valid until its CQE. This method moves each `Vec<u8>` into a fixed slot
    /// before constructing the SQE. Slots are never resized, no buffer contents
    /// are mutated while in flight, and `drain_owned` takes a slot only after
    /// observing its unique CQE. `Drop` synchronously drains any remaining slots
    /// before their buffers are released, so callers cannot end the lifetime
    /// early even by dropping the writer.
    #[inline(never)]
    pub fn submit_owned(&mut self, jobs: &mut Vec<OwnedSendJob>) -> io::Result<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        if jobs.len() > self.available_owned_slots() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "io_uring owned-send registry is full",
            ));
        }
        if jobs.iter().any(|job| job.start >= job.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "owned send must contain at least one unsent byte",
            ));
        }

        let owned = std::mem::take(jobs);
        let mut slot_ids = Vec::with_capacity(owned.len());
        for job in owned {
            let slot = self
                .free_owned_slots
                .pop()
                .expect("capacity checked before moving owned sends");
            debug_assert!(self.owned_slots[slot].is_none());
            self.owned_slots[slot] = Some(job);
            slot_ids.push(slot);
        }

        let entries: Vec<_> = slot_ids
            .iter()
            .map(|&slot| {
                let job = self.owned_slots[slot]
                    .as_ref()
                    .expect("slot populated before SQE construction");
                let pending = &job.bytes[job.start..];
                opcode::Send::new(
                    types::Fd(job.fd),
                    pending.as_ptr(),
                    pending.len().min(u32::MAX as usize) as u32,
                )
                .flags(libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL)
                .build()
                .user_data(slot as u64)
            })
            .collect();
        {
            let mut sq = self.ring.submission();
            // Each pointer addresses a heap allocation owned by the
            // corresponding fixed `owned_slots` entry. `push_entries` is
            // all-or-nothing; successful entries stay owned until their CQEs,
            // and the error path below restores every job before returning.
            let pushed = push_entries(&mut sq, &entries);
            if pushed.is_err() {
                for slot in slot_ids {
                    jobs.push(
                        self.owned_slots[slot]
                            .take()
                            .expect("restore populated slot after failed push"),
                    );
                    self.free_owned_slots.push(slot);
                }
                return Err(io::Error::other("io_uring submission queue full"));
            }
            sq.sync();
        }

        // Publication has transferred buffer-lifetime responsibility to
        // `owned_slots`, but one `io_uring_enter` may consume only a prefix of
        // the SQ. Keep entering until every new SQE is kernel-owned; otherwise a
        // quiet event loop could strand an unsubmitted job indefinitely. EINTR
        // is retryable. Any other enter failure after a prefix was consumed is
        // not safely recoverable, so keep the opt-in path fail-closed.
        let mut submitted = 0usize;
        while submitted < entries.len() {
            match self.ring.submit() {
                Ok(0) => {
                    eprintln!(
                        "fatal: io_uring_enter consumed zero entries from a non-empty owned-send SQ"
                    );
                    std::process::abort();
                }
                Ok(count) => {
                    submitted = submitted
                        .checked_add(count)
                        .expect("io_uring submitted-count overflow");
                    if submitted > entries.len() {
                        eprintln!("fatal: io_uring reported more submissions than were published");
                        std::process::abort();
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    eprintln!(
                        "fatal: io_uring_enter failed after publishing owned send buffers: {err}"
                    );
                    std::process::abort();
                }
            }
        }
        Ok(())
    }

    /// Drain every currently available owned-send CQE without entering or
    /// waiting in the kernel.
    ///
    /// Completion order is arbitrary; `tag` restores caller identity. Each
    /// returned buffer is again exclusively owned by the caller.
    #[inline(never)]
    pub fn drain_owned(&mut self, out: &mut Vec<OwnedSendCompletion>) {
        out.clear();
        let (ring, slots, free_slots) = (
            &mut self.ring,
            &mut self.owned_slots,
            &mut self.free_owned_slots,
        );
        let mut cq = ring.completion();
        for cqe in &mut cq {
            let slot = cqe.user_data() as usize;
            let Some(job) = slots.get_mut(slot).and_then(Option::take) else {
                eprintln!("fatal: io_uring returned an unknown or duplicate owned-send slot");
                std::process::abort();
            };
            free_slots.push(slot);
            out.push(OwnedSendCompletion {
                tag: job.tag,
                bytes: job.bytes,
                start: job.start,
                outcome: SendOutcome::from_cqe_result(cqe.result()),
            });
        }
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
    /// This borrowed API is the synchronous reference path. Production callers
    /// that keep operations in flight across calls use [`Self::submit_owned`],
    /// whose registry carries the corresponding buffer-lifetime proof.
    #[inline(never)]
    pub fn send_batch(
        &mut self,
        jobs: &[SendJob<'_>],
        out: &mut Vec<SendOutcome>,
    ) -> io::Result<()> {
        out.clear();
        if self.has_owned_in_flight() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cannot use borrowed sends while owned sends are in flight",
            ));
        }
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
            // See the soundness argument on `send_batch`. `push_entries` is
            // all-or-nothing, so its error path cannot publish a prefix whose
            // borrowed buffer pointers would outlive this call. On success every
            // SQE is completed and drained below before `submit_chunk` returns.
            if push_entries(&mut sq, &entries).is_err() {
                return Err(io::Error::other("io_uring submission queue full"));
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

impl Drop for BatchWriter {
    fn drop(&mut self) {
        let mut completed = Vec::new();
        while self.has_owned_in_flight() {
            loop {
                match self.ring.submit_and_wait(1) {
                    Ok(_) => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        eprintln!(
                            "fatal: failed to drain owned io_uring sends during shutdown: {err}"
                        );
                        std::process::abort();
                    }
                }
            }
            self.drain_owned(&mut completed);
        }
    }
}

/// A reusable `io_uring` instance for readiness-gated batched socket receives.
///
/// Each buffer is initialized once and then reused by its connection. The
/// reader owns all in-flight buffers, so neither connection teardown nor an
/// event-loop iteration can invalidate a pointer retained by the kernel.
pub struct BatchReader {
    ring: IoUring,
    entries: usize,
    // Fixed after construction. The Vec allocation stored in a populated slot
    // remains at one address until the corresponding CQE is observed.
    owned_slots: Vec<Option<OwnedRecvJob>>,
    free_owned_slots: Vec<usize>,
}

impl BatchReader {
    /// Create a receive ring with `entries` submission slots.
    pub fn new(entries: u32) -> io::Result<Self> {
        let ring = IoUring::new(entries)?;
        let entries = entries as usize;
        Ok(Self {
            ring,
            entries,
            owned_slots: (0..entries).map(|_| None).collect(),
            free_owned_slots: (0..entries).rev().collect(),
        })
    }

    /// Number of receives that can be published without waiting for a CQE.
    #[must_use]
    pub fn available_owned_slots(&self) -> usize {
        self.free_owned_slots.len()
    }

    /// Whether at least one receive is waiting for a CQE.
    #[must_use]
    pub fn has_owned_in_flight(&self) -> bool {
        self.free_owned_slots.len() != self.entries
    }

    /// Publish owned receives without waiting for their completions.
    ///
    /// On success `jobs` is empty. On a recoverable validation or SQ-capacity
    /// failure, no SQE was published and `jobs` is restored unchanged.
    ///
    /// # Soundness
    ///
    /// Every receive buffer is a fully initialized `Vec<u8>` moved into a fixed
    /// registry slot before its pointer is published. No Rust reference to its
    /// contents exists while the operation is in flight. The slot is taken only
    /// after its unique CQE, and `Drop` drains all remaining operations before
    /// releasing their allocations.
    #[inline(never)]
    pub fn submit_owned(&mut self, jobs: &mut Vec<OwnedRecvJob>) -> io::Result<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        if jobs.len() > self.available_owned_slots() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "io_uring owned-receive registry is full",
            ));
        }
        if jobs.iter().any(|job| job.buffer.is_empty()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "owned receive must provide non-empty initialized storage",
            ));
        }

        let owned = std::mem::take(jobs);
        let mut slot_ids = Vec::with_capacity(owned.len());
        for job in owned {
            let slot = self
                .free_owned_slots
                .pop()
                .expect("capacity checked before moving owned receives");
            debug_assert!(self.owned_slots[slot].is_none());
            self.owned_slots[slot] = Some(job);
            slot_ids.push(slot);
        }

        let mut entries = Vec::with_capacity(slot_ids.len());
        for &slot in &slot_ids {
            let job = self.owned_slots[slot]
                .as_mut()
                .expect("slot populated before receive SQE construction");
            entries.push(
                opcode::Recv::new(
                    types::Fd(job.fd),
                    job.buffer.as_mut_ptr(),
                    job.buffer.len().min(u32::MAX as usize) as u32,
                )
                .flags(libc::MSG_DONTWAIT)
                .build()
                .user_data(slot as u64),
            );
        }

        {
            let mut sq = self.ring.submission();
            // Every mutable pointer addresses initialized storage owned by its
            // fixed registry slot. `push_entries` is all-or-nothing, so the
            // error path can restore every job before returning.
            if push_entries(&mut sq, &entries).is_err() {
                for slot in slot_ids {
                    jobs.push(
                        self.owned_slots[slot]
                            .take()
                            .expect("restore populated receive slot after failed push"),
                    );
                    self.free_owned_slots.push(slot);
                }
                return Err(io::Error::other("io_uring submission queue full"));
            }
            sq.sync();
        }

        // Once SQEs are published, their buffers must remain owned until the
        // kernel consumes every entry. Retry EINTR; fail closed on an enter
        // error that cannot safely return ownership to the caller.
        let mut submitted = 0usize;
        while submitted < entries.len() {
            match self.ring.submit() {
                Ok(0) => {
                    eprintln!(
                        "fatal: io_uring_enter consumed zero entries from a non-empty receive SQ"
                    );
                    std::process::abort();
                }
                Ok(count) => {
                    submitted = submitted
                        .checked_add(count)
                        .expect("io_uring receive submitted-count overflow");
                    if submitted > entries.len() {
                        eprintln!(
                            "fatal: io_uring reported more receive submissions than were published"
                        );
                        std::process::abort();
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    eprintln!(
                        "fatal: io_uring_enter failed after publishing owned receive buffers: {err}"
                    );
                    std::process::abort();
                }
            }
        }
        Ok(())
    }

    /// Drain every currently available receive CQE without waiting.
    #[inline(never)]
    pub fn drain_owned(&mut self, out: &mut Vec<OwnedRecvCompletion>) {
        out.clear();
        let (ring, slots, free_slots) = (
            &mut self.ring,
            &mut self.owned_slots,
            &mut self.free_owned_slots,
        );
        let mut cq = ring.completion();
        for cqe in &mut cq {
            let slot = cqe.user_data() as usize;
            let Some(job) = slots.get_mut(slot).and_then(Option::take) else {
                eprintln!("fatal: io_uring returned an unknown or duplicate receive slot");
                std::process::abort();
            };
            free_slots.push(slot);
            out.push(OwnedRecvCompletion {
                tag: job.tag,
                buffer: job.buffer,
                outcome: RecvOutcome::from_cqe_result(cqe.result()),
            });
        }
    }
}

impl Drop for BatchReader {
    fn drop(&mut self) {
        let mut completed = Vec::new();
        while self.has_owned_in_flight() {
            loop {
                match self.ring.submit_and_wait(1) {
                    Ok(_) => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        eprintln!(
                            "fatal: failed to drain owned io_uring receives during shutdown: {err}"
                        );
                        std::process::abort();
                    }
                }
            }
            self.drain_owned(&mut completed);
        }
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

    fn wait_for_owned(writer: &mut BatchWriter, want: usize) -> Vec<OwnedSendCompletion> {
        let mut all = Vec::with_capacity(want);
        let mut available = Vec::new();
        while all.len() < want {
            loop {
                match writer.ring.submit_and_wait(1) {
                    Ok(_) => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => panic!("wait for owned completion: {err}"),
                }
            }
            writer.drain_owned(&mut available);
            all.append(&mut available);
        }
        all
    }

    fn wait_for_owned_recvs(reader: &mut BatchReader, want: usize) -> Vec<OwnedRecvCompletion> {
        let mut all = Vec::with_capacity(want);
        let mut available = Vec::new();
        while all.len() < want {
            loop {
                match reader.ring.submit_and_wait(1) {
                    Ok(_) => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => panic!("wait for owned receive completion: {err}"),
                }
            }
            reader.drain_owned(&mut available);
            all.append(&mut available);
        }
        all
    }

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
    fn batched_receives_return_every_buffer_and_tag() {
        if !is_available() {
            return;
        }
        let mut reader = BatchReader::new(8).expect("ring");
        let payloads: [&[u8]; 4] = [b"PING", b"set key value", b"get key", b"quit"];
        let mut pairs: Vec<(UnixStream, UnixStream)> = (0..payloads.len())
            .map(|_| UnixStream::pair().expect("socketpair"))
            .collect();
        for ((tx, _rx), payload) in pairs.iter_mut().zip(payloads) {
            tx.write_all(payload).expect("prime receive socket");
        }
        let mut jobs: Vec<OwnedRecvJob> = pairs
            .iter()
            .enumerate()
            .map(|(tag, (_tx, rx))| OwnedRecvJob {
                tag: u64::try_from(tag).expect("test tag fits"),
                fd: rx.as_raw_fd(),
                buffer: vec![0; 64],
            })
            .collect();

        reader.submit_owned(&mut jobs).expect("submit receives");
        assert!(jobs.is_empty(), "ownership must transfer on success");
        let mut completed = wait_for_owned_recvs(&mut reader, payloads.len());
        completed.sort_unstable_by_key(|completion| completion.tag);
        for (index, completion) in completed.iter().enumerate() {
            let RecvOutcome::Read(count) = completion.outcome else {
                panic!("receive {index} failed: {:?}", completion.outcome);
            };
            assert_eq!(&completion.buffer[..count], payloads[index]);
        }
        assert!(!reader.has_owned_in_flight());
        assert_eq!(reader.available_owned_slots(), 8);
    }

    #[test]
    fn owned_receive_reports_would_block_and_orderly_close() {
        if !is_available() {
            return;
        }
        let mut reader = BatchReader::new(8).expect("ring");
        let (_open_tx, open_rx) = UnixStream::pair().expect("open socketpair");
        let (closed_tx, closed_rx) = UnixStream::pair().expect("closed socketpair");
        drop(closed_tx);
        let mut jobs = vec![
            OwnedRecvJob {
                tag: 1,
                fd: open_rx.as_raw_fd(),
                buffer: vec![0; 32],
            },
            OwnedRecvJob {
                tag: 2,
                fd: closed_rx.as_raw_fd(),
                buffer: vec![0; 32],
            },
        ];

        reader.submit_owned(&mut jobs).expect("submit receives");
        let mut completed = wait_for_owned_recvs(&mut reader, 2);
        completed.sort_unstable_by_key(|completion| completion.tag);
        assert_eq!(completed[0].outcome, RecvOutcome::WouldBlock);
        assert_eq!(completed[1].outcome, RecvOutcome::Closed);
        assert!(
            completed
                .iter()
                .all(|completion| completion.buffer.len() == 32)
        );
    }

    #[test]
    fn owned_receive_validation_and_capacity_errors_preserve_jobs() {
        if !is_available() {
            return;
        }
        let mut reader = BatchReader::new(1).expect("ring");
        let pairs: Vec<(UnixStream, UnixStream)> = (0..2)
            .map(|_| UnixStream::pair().expect("socketpair"))
            .collect();
        let mut jobs: Vec<OwnedRecvJob> = pairs
            .iter()
            .enumerate()
            .map(|(tag, (_tx, rx))| OwnedRecvJob {
                tag: u64::try_from(tag).expect("test tag fits"),
                fd: rx.as_raw_fd(),
                buffer: vec![0; 8],
            })
            .collect();
        let err = reader
            .submit_owned(&mut jobs)
            .expect_err("capacity refusal");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(jobs.len(), 2);

        jobs.truncate(1);
        jobs[0].buffer.clear();
        let err = reader
            .submit_owned(&mut jobs)
            .expect_err("empty buffer refusal");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].buffer.is_empty());
        assert_eq!(reader.available_owned_slots(), 1);
    }

    #[test]
    fn owned_sends_return_each_buffer_and_tag_after_completion() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let payloads = [
            (41, b"+OK\r\n".to_vec()),
            (7, b"$5\r\nhello\r\n".to_vec()),
            (999, b":42\r\n".to_vec()),
        ];
        let pairs: Vec<(UnixStream, UnixStream)> = (0..payloads.len())
            .map(|_| UnixStream::pair().expect("socketpair"))
            .collect();
        let mut jobs: Vec<OwnedSendJob> = pairs
            .iter()
            .zip(payloads.iter())
            .map(|((tx, _rx), (tag, bytes))| OwnedSendJob {
                tag: *tag,
                fd: tx.as_raw_fd(),
                bytes: bytes.clone(),
                start: 0,
            })
            .collect();

        writer.submit_owned(&mut jobs).expect("submit owned");
        assert!(jobs.is_empty(), "ownership must transfer on success");
        assert!(writer.has_owned_in_flight());

        let mut completed = wait_for_owned(&mut writer, payloads.len());
        completed.sort_unstable_by_key(|completion| completion.tag);
        for completion in &completed {
            let (_, expected) = payloads
                .iter()
                .find(|(tag, _)| *tag == completion.tag)
                .expect("known tag");
            assert_eq!(completion.start, 0);
            assert_eq!(completion.bytes, *expected);
            assert_eq!(
                completion.outcome,
                SendOutcome::Wrote(expected.len()),
                "tag {}",
                completion.tag
            );
        }
        assert!(!writer.has_owned_in_flight());
        assert_eq!(writer.available_owned_slots(), 8);

        for (index, (_tx, rx)) in pairs.iter().enumerate() {
            let mut actual = vec![0; payloads[index].1.len()];
            (&*rx).read_exact(&mut actual).expect("read payload");
            assert_eq!(actual, payloads[index].1);
        }
    }

    #[test]
    fn owned_send_validation_and_capacity_errors_preserve_jobs() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(2).expect("ring");
        let pairs: Vec<(UnixStream, UnixStream)> = (0..3)
            .map(|_| UnixStream::pair().expect("socketpair"))
            .collect();
        let mut too_many: Vec<OwnedSendJob> = pairs
            .iter()
            .enumerate()
            .map(|(tag, (tx, _rx))| OwnedSendJob {
                tag: u64::try_from(tag).expect("test tag fits in u64"),
                fd: tx.as_raw_fd(),
                bytes: vec![b'x'],
                start: 0,
            })
            .collect();
        let err = writer
            .submit_owned(&mut too_many)
            .expect_err("capacity refusal");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(too_many.len(), 3);
        assert_eq!(writer.available_owned_slots(), 2);

        too_many.truncate(1);
        too_many[0].start = too_many[0].bytes.len();
        let err = writer
            .submit_owned(&mut too_many)
            .expect_err("invalid cursor");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(too_many.len(), 1);
        assert_eq!(too_many[0].bytes, b"x");
        assert_eq!(writer.available_owned_slots(), 2);
    }

    #[test]
    fn borrowed_send_is_refused_while_owned_buffer_is_in_flight() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let (tx, mut rx) = UnixStream::pair().expect("socketpair");
        let mut owned = vec![OwnedSendJob {
            tag: 1,
            fd: tx.as_raw_fd(),
            bytes: b"owned".to_vec(),
            start: 0,
        }];
        writer.submit_owned(&mut owned).expect("submit owned");

        let borrowed = [SendJob {
            fd: tx.as_raw_fd(),
            bytes: b"borrowed",
        }];
        let mut outcomes = Vec::new();
        let err = writer
            .send_batch(&borrowed, &mut outcomes)
            .expect_err("mixed lifetime modes");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        let completed = wait_for_owned(&mut writer, 1);
        assert_eq!(completed[0].bytes, b"owned");
        let mut actual = [0; 5];
        rx.read_exact(&mut actual).expect("read owned payload");
        assert_eq!(&actual, b"owned");
    }

    #[test]
    fn owned_send_closed_peer_returns_failed_with_buffer_intact() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let (tx, rx) = UnixStream::pair().expect("socketpair");
        drop(rx);
        let mut jobs = vec![OwnedSendJob {
            tag: 17,
            fd: tx.as_raw_fd(),
            bytes: b"payload".to_vec(),
            start: 2,
        }];

        writer.submit_owned(&mut jobs).expect("submit owned");
        let completed = wait_for_owned(&mut writer, 1);
        assert_eq!(completed[0].tag, 17);
        assert_eq!(completed[0].bytes, b"payload");
        assert_eq!(completed[0].start, 2);
        assert!(matches!(completed[0].outcome, SendOutcome::Failed(_)));
    }

    #[test]
    fn closing_source_fd_after_submit_keeps_buffer_alive_through_cqe() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let (tx, mut rx) = UnixStream::pair().expect("socketpair");
        let mut jobs = vec![OwnedSendJob {
            tag: 23,
            fd: tx.as_raw_fd(),
            bytes: b"cancel-safe".to_vec(),
            start: 0,
        }];

        writer.submit_owned(&mut jobs).expect("submit owned");
        drop(tx);
        let completed = wait_for_owned(&mut writer, 1);

        assert_eq!(completed[0].tag, 23);
        assert_eq!(completed[0].bytes, b"cancel-safe");
        assert_eq!(completed[0].outcome, SendOutcome::Wrote(11));
        let mut actual = [0; 11];
        rx.read_exact(&mut actual).expect("read submitted payload");
        assert_eq!(&actual, b"cancel-safe");
    }

    #[test]
    fn cancellation_race_keeps_owned_buffer_until_original_cqe() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let (mut tx, _rx) = UnixStream::pair().expect("socketpair");
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

        let mut jobs = vec![OwnedSendJob {
            tag: 29,
            fd: tx.as_raw_fd(),
            bytes: b"cancel-race".to_vec(),
            start: 0,
        }];
        writer.submit_owned(&mut jobs).expect("submit owned");

        // The first registry allocation uses slot/user_data 0. A nonblocking
        // SEND may post EAGAIN before the synchronous cancel reaches it; both
        // outcomes are valid, but neither may release the owned allocation
        // before the original SEND CQE is observed below.
        match writer.ring.submitter().register_sync_cancel(
            Some(types::Timespec::new().sec(1)),
            types::CancelBuilder::user_data(0),
        ) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => panic!("cancel owned send: {err}"),
        }

        let completed = wait_for_owned(&mut writer, 1);
        assert_eq!(completed[0].tag, 29);
        assert_eq!(completed[0].bytes, b"cancel-race");
        assert!(matches!(
            completed[0].outcome,
            SendOutcome::WouldBlock | SendOutcome::Failed(libc::ECANCELED)
        ));
        assert!(!writer.has_owned_in_flight());
    }

    #[test]
    fn owned_send_saturated_socket_returns_prompt_would_block() {
        if !is_available() {
            return;
        }
        let mut writer = BatchWriter::new(8).expect("ring");
        let (mut tx, _rx) = UnixStream::pair().expect("socketpair");
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

        let mut jobs = vec![OwnedSendJob {
            tag: 22,
            fd: tx.as_raw_fd(),
            bytes: b"xyz".to_vec(),
            start: 1,
        }];
        let started = Instant::now();
        writer.submit_owned(&mut jobs).expect("submit owned");
        let completed = wait_for_owned(&mut writer, 1);
        let elapsed = started.elapsed();

        assert_eq!(completed[0].bytes, b"xyz");
        assert_eq!(completed[0].start, 1);
        assert_eq!(completed[0].outcome, SendOutcome::WouldBlock);
        assert!(
            elapsed < Duration::from_millis(250),
            "saturated owned send internally polled for {elapsed:?}"
        );
    }

    #[test]
    fn dropping_writer_drains_owned_send_before_releasing_buffer() {
        if !is_available() {
            return;
        }
        let (tx, mut rx) = UnixStream::pair().expect("socketpair");
        {
            let mut writer = BatchWriter::new(8).expect("ring");
            let mut jobs = vec![OwnedSendJob {
                tag: 1,
                fd: tx.as_raw_fd(),
                bytes: b"drop-safe".to_vec(),
                start: 0,
            }];
            writer.submit_owned(&mut jobs).expect("submit owned");
        }

        let mut actual = [0; 9];
        rx.read_exact(&mut actual).expect("read after writer drop");
        assert_eq!(&actual, b"drop-safe");
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
