//! Batched socket submission via `io_uring`, isolated behind a safe API.
//!
//! # Why this crate exists
//!
//! `fr-server` declares `#![forbid(unsafe_code)]`, and `forbid` cannot be relaxed
//! per-module. The `io_uring` submission-queue push is inherently unsafe (the
//! kernel reads the caller's buffers asynchronously), so per `AGENTS.md` — *"if
//! narrow unsafe usage is unavoidable, isolate it behind audited interfaces and
//! tests"* — the entire unsafe surface lives here, with the soundness arguments
//! written out and covered by tests.
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
//! The write side groups ready connections behind one SQ publication. The read
//! side goes further: each connection gets one multishot receive backed by a
//! worker-local provided-buffer ring, eliminating both the per-event `read` and
//! the per-message receive submission. The feature and runtime flag remain
//! default-off while this implementation is evaluated.
//!
//! At `pipeline=16` there is nothing here to win — that census measures 0.13-0.16
//! syscalls per operation for both engines, because a whole pipelined batch
//! already coalesces into one write. See the 2026-07-25 entry in
//! `docs/NEGATIVE_EVIDENCE.md`. This crate targets the unpipelined regime only.

use std::collections::HashSet;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU16, Ordering};

use io_uring::{IoUring, cqueue, opcode, squeue, types};

/// The send path's single unsafe operation. Both callers establish that every
/// SQE buffer remains valid through its CQE before reaching this helper.
fn push_entries(
    sq: &mut squeue::SubmissionQueue<'_>,
    entries: &[squeue::Entry],
) -> Result<(), squeue::PushError> {
    // SAFETY: this private helper is called only from `submit_owned`, whose fixed
    // registry owns every buffer through completion, and `submit_chunk`, whose
    // synchronous drain keeps every borrow alive through completion.
    unsafe { sq.push_multiple(entries) }
}

/// Default submission/completion queue depth. Sized to comfortably exceed the
/// ~48 ready connections a single `epoll_wait` reports under `-c50`, so the
/// common batch never has to be chunked.
pub const DEFAULT_RING_ENTRIES: u32 = 256;

/// Number of kernel-selectable receive buffers owned by each worker ring.
///
/// This is deliberately larger than the realistic per-worker connection count
/// (128 clients / 16 workers = 8) so a burst can post many CQEs before userspace
/// drains and recycles the buffers without terminating a multishot request with
/// `ENOBUFS`. The value must remain a power of two for the buffer-ring mask.
pub const MULTISHOT_BUFFER_COUNT: usize = 256;

/// Size of each provided receive buffer. Matches the server's prior stack read
/// buffer, preserving its batching and query-limit behavior.
pub const MULTISHOT_BUFFER_LEN: usize = 8192;

const MULTISHOT_BUFFER_GROUP: u16 = 1;
const CANCEL_USER_DATA_BIT: u64 = 1 << 63;

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

/// One completion from a multishot receive request.
pub struct MultishotRecvEvent<'a> {
    /// Caller-defined connection identity supplied to [`MultishotReceiver::arm`].
    pub tag: u64,
    /// Data or terminal status carried by this CQE.
    pub outcome: MultishotRecvOutcome<'a>,
    /// True when the kernel retired the multishot request. An open connection
    /// should arm a replacement after the current completion batch is drained.
    pub request_ended: bool,
}

/// Result carried by one multishot receive CQE.
pub enum MultishotRecvOutcome<'a> {
    /// Newly received bytes. The slice is valid only for the duration of the
    /// drain callback; its provided buffer is recycled immediately afterward.
    Data(&'a [u8]),
    /// Orderly peer shutdown.
    Closed,
    /// A transient condition (`EAGAIN`, `EINTR`, or buffer-pool exhaustion).
    Retry,
    /// Cancellation requested by the owner during connection teardown.
    Cancelled,
    /// A non-retryable error; the value is a positive errno.
    Failed(i32),
}

/// Page-aligned storage for an `io_uring` provided-buffer ring.
///
/// The kernel ABI requires page alignment. Keeping this allocation boxed also
/// prevents its address from changing while registered.
#[repr(C, align(4096))]
struct AlignedBufferRing {
    entries: [MaybeUninit<types::BufRingEntry>; MULTISHOT_BUFFER_COUNT],
}

struct ProvidedBufferRing {
    ring: Box<AlignedBufferRing>,
    buffers: Vec<Box<[u8]>>,
    tail: u16,
}

impl ProvidedBufferRing {
    fn new() -> Self {
        let ring = Box::new(AlignedBufferRing {
            // `io_uring_buf` consists only of integer fields, so its all-zero
            // representation is valid. Entries are fully initialized before
            // their publication through the shared tail.
            entries: [const { MaybeUninit::zeroed() }; MULTISHOT_BUFFER_COUNT],
        });
        let buffers = (0..MULTISHOT_BUFFER_COUNT)
            .map(|_| vec![0u8; MULTISHOT_BUFFER_LEN].into_boxed_slice())
            .collect();
        let mut provided = Self {
            ring,
            buffers,
            tail: 0,
        };
        for bid in 0..MULTISHOT_BUFFER_COUNT {
            provided.recycle(bid as u16);
        }
        provided
    }

    fn base(&self) -> *const types::BufRingEntry {
        self.ring.entries.as_ptr().cast()
    }

    fn base_mut(&mut self) -> *mut types::BufRingEntry {
        self.ring.entries.as_mut_ptr().cast()
    }

    fn recycle(&mut self, bid: u16) {
        let bid_index = usize::from(bid);
        if bid_index >= self.buffers.len() {
            eprintln!("fatal: io_uring returned an out-of-range provided-buffer id");
            std::process::abort();
        }
        let slot = usize::from(self.tail) & (MULTISHOT_BUFFER_COUNT - 1);
        let buffer_ptr = self.buffers[bid_index].as_mut_ptr();
        // SAFETY: `slot` is masked into the page-aligned allocation. The entry
        // was zero-initialized, is no longer kernel-owned after its CQE, and is
        // not made visible again until the release-store to `tail` below.
        let entry = unsafe { &mut *self.base_mut().add(slot) };
        entry.set_addr(buffer_ptr as u64);
        entry.set_len(MULTISHOT_BUFFER_LEN as u32);
        entry.set_bid(bid);
        self.tail = self.tail.wrapping_add(1);

        // SAFETY: `base` points to the first valid, page-aligned buffer-ring
        // entry. `BufRingEntry::tail` returns its ABI-defined u16 tail field,
        // which is naturally aligned. The release-store publishes the entry
        // writes before the kernel can consume the new tail.
        let tail = unsafe { types::BufRingEntry::tail(self.base()) };
        let atomic_tail = tail.cast::<AtomicU16>();
        // SAFETY: the kernel and this worker are the only participants. The
        // worker is the sole writer; the kernel performs the paired acquire.
        unsafe { &*atomic_tail }.store(self.tail, Ordering::Release);
    }
}

/// A worker-local receiver with one persistent multishot request per socket.
///
/// Each request consumes initialized storage from a registered provided-buffer
/// ring and can produce arbitrarily many CQEs without another submission. The
/// ring fd is pollable, allowing the server to remove client sockets from its
/// epoll read set entirely.
pub struct MultishotReceiver {
    // Declared before `provided` deliberately: after `Drop`, the ring fd closes
    // before the registered memory and receive buffers are released.
    ring: IoUring,
    provided: ProvidedBufferRing,
    active: HashSet<u64>,
}

impl MultishotReceiver {
    /// Create a receiver ring and register its page-aligned provided buffers.
    pub fn new(entries: u32) -> io::Result<Self> {
        let ring = IoUring::new(entries)?;
        let provided = ProvidedBufferRing::new();
        // SAFETY: `provided.ring` is boxed and therefore address-stable. It is
        // stored in `Self` until `Drop` unregisters the group, and the entry
        // count exactly matches the allocation.
        unsafe {
            ring.submitter().register_buf_ring_with_flags(
                provided.base() as u64,
                MULTISHOT_BUFFER_COUNT as u16,
                MULTISHOT_BUFFER_GROUP,
                0,
            )?;
        }
        Ok(Self {
            ring,
            provided,
            active: HashSet::new(),
        })
    }

    /// File descriptor that becomes readable when receive CQEs are available.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.ring.as_raw_fd()
    }

    /// Whether `tag` currently has a live multishot request.
    #[must_use]
    pub fn is_active(&self, tag: u64) -> bool {
        self.active.contains(&tag)
    }

    /// Arm one persistent receive for `fd`.
    pub fn arm(&mut self, tag: u64, fd: RawFd) -> io::Result<()> {
        if tag & CANCEL_USER_DATA_BIT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "multishot receive tag reserves its high bit",
            ));
        }
        if self.active.contains(&tag) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "multishot receive tag is already armed",
            ));
        }
        let entry = opcode::RecvMulti::new(types::Fd(fd), MULTISHOT_BUFFER_GROUP)
            .build()
            .user_data(tag);
        self.submit_one(entry, "multishot receive")?;
        self.active.insert(tag);
        Ok(())
    }

    /// Asynchronously cancel a live request during connection teardown.
    pub fn cancel(&mut self, tag: u64) -> io::Result<()> {
        if !self.active.contains(&tag) {
            return Ok(());
        }
        let entry = opcode::AsyncCancel::new(tag)
            .build()
            .user_data(tag | CANCEL_USER_DATA_BIT);
        self.submit_one(entry, "multishot receive cancellation")
    }

    /// Drain every currently available CQE. Provided buffers are lent to the
    /// callback and recycled immediately after it returns.
    pub fn drain(&mut self, mut apply: impl FnMut(MultishotRecvEvent<'_>)) {
        let (ring, provided, active) = (&mut self.ring, &mut self.provided, &mut self.active);
        let mut cq = ring.completion();
        for cqe in &mut cq {
            let tag = cqe.user_data();
            if tag & CANCEL_USER_DATA_BIT != 0 {
                // The original receive CQE owns request retirement and any
                // selected buffer. The cancellation CQE is only an ack.
                continue;
            }

            let flags = cqe.flags();
            let request_ended = !cqueue::more(flags);
            if request_ended {
                active.remove(&tag);
            }
            let selected = cqueue::buffer_select(flags);
            let result = cqe.result();
            if result > 0 {
                let Some(bid) = selected else {
                    eprintln!("fatal: multishot receive data CQE omitted its selected buffer");
                    std::process::abort();
                };
                let count = result as usize;
                let Some(buffer) = provided.buffers.get(usize::from(bid)) else {
                    eprintln!("fatal: multishot receive selected an unknown buffer");
                    std::process::abort();
                };
                if count > buffer.len() {
                    eprintln!("fatal: multishot receive exceeded its selected buffer");
                    std::process::abort();
                }
                apply(MultishotRecvEvent {
                    tag,
                    outcome: MultishotRecvOutcome::Data(&buffer[..count]),
                    request_ended,
                });
            } else {
                let outcome = if result == 0 {
                    MultishotRecvOutcome::Closed
                } else {
                    let errno = -result;
                    if errno == libc::EAGAIN
                        || errno == libc::EWOULDBLOCK
                        || errno == libc::EINTR
                        || errno == libc::ENOBUFS
                    {
                        MultishotRecvOutcome::Retry
                    } else if errno == libc::ECANCELED {
                        MultishotRecvOutcome::Cancelled
                    } else {
                        MultishotRecvOutcome::Failed(errno)
                    }
                };
                apply(MultishotRecvEvent {
                    tag,
                    outcome,
                    request_ended,
                });
            }
            if let Some(bid) = selected {
                provided.recycle(bid);
            }
        }
    }

    fn submit_one(&mut self, entry: squeue::Entry, operation: &str) -> io::Result<()> {
        {
            let mut sq = self.ring.submission();
            if push_entries(&mut sq, std::slice::from_ref(&entry)).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("io_uring submission queue full while arming {operation}"),
                ));
            }
            sq.sync();
        }
        loop {
            match self.ring.submit() {
                Ok(0) => {
                    eprintln!("fatal: io_uring consumed zero entries while arming {operation}");
                    std::process::abort();
                }
                Ok(_) => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => {
                    eprintln!("fatal: io_uring_enter failed after publishing {operation}: {err}");
                    std::process::abort();
                }
            }
        }
    }
}

impl Drop for MultishotReceiver {
    fn drop(&mut self) {
        let active: Vec<u64> = self.active.iter().copied().collect();
        for tag in active {
            if let Err(err) = self.cancel(tag) {
                eprintln!("fatal: failed to submit multishot receive cancellation: {err}");
                std::process::abort();
            }
        }
        while !self.active.is_empty() {
            loop {
                match self.ring.submit_and_wait(1) {
                    Ok(_) => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        eprintln!("fatal: failed to drain multishot receives: {err}");
                        std::process::abort();
                    }
                }
            }
            self.drain(|_| {});
        }
        if let Err(err) = self
            .ring
            .submitter()
            .unregister_buf_ring(MULTISHOT_BUFFER_GROUP)
        {
            eprintln!("warn: failed to unregister io_uring provided-buffer ring: {err}");
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
    fn multishot_receive_reuses_one_request_for_multiple_messages() {
        if !is_available() {
            return;
        }
        let Ok(mut receiver) = MultishotReceiver::new(8) else {
            // A pre-6.0 kernel can support basic io_uring but not multishot
            // receive or registered buffer rings.
            return;
        };
        let (mut tx, rx) = UnixStream::pair().expect("socketpair");
        receiver.arm(7, rx.as_raw_fd()).expect("arm multishot");

        tx.write_all(b"first").expect("write first");
        receiver.ring.submit_and_wait(1).expect("wait first CQE");
        let mut first = Vec::new();
        receiver.drain(|event| {
            assert_eq!(event.tag, 7);
            if let MultishotRecvOutcome::Data(bytes) = event.outcome {
                first.extend_from_slice(bytes);
            }
        });
        assert_eq!(first, b"first");
        assert!(receiver.is_active(7), "first CQE must retain the request");

        tx.write_all(b"second").expect("write second");
        receiver.ring.submit_and_wait(1).expect("wait second CQE");
        let mut second = Vec::new();
        receiver.drain(|event| {
            assert_eq!(event.tag, 7);
            if let MultishotRecvOutcome::Data(bytes) = event.outcome {
                second.extend_from_slice(bytes);
            }
        });
        assert_eq!(second, b"second");
        assert!(
            receiver.is_active(7),
            "second CQE must still use the original request"
        );

        drop(tx);
        receiver.ring.submit_and_wait(1).expect("wait close CQE");
        let mut closed = false;
        receiver.drain(|event| {
            if matches!(event.outcome, MultishotRecvOutcome::Closed) {
                closed = true;
                assert!(event.request_ended);
            }
        });
        assert!(closed, "peer shutdown must surface as a close CQE");
        assert!(!receiver.is_active(7));
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
