//! Reporting for operations long enough that somebody has to be told about
//! them, and the seam through which they can be abandoned.
//!
//! A copy that reports nothing cannot show progress, and a copy that cannot be
//! stopped cannot offer a Cancel button — so both live on the same trait, at
//! the point where the bytes are actually moving. This is deliberately *not*
//! in the operations queue: the queue knows what it asked for, and only the
//! backend knows how much of it has landed.
//!
//! Every method has a default, so a caller that only wants a byte count
//! implements one and a caller that only wants a cancel flag implements one,
//! and [`Vfs::copy_observed`]:
//!
//! ```no_run
//! # use ferret_vfs::{Checkpoint, ProgressReporter, Vfs as _, VfsPath};
//! # use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
//! # struct Counted { bytes: AtomicU64, stop: AtomicBool }
//! # impl ProgressReporter for Counted {
//! #     fn bytes(&self, count: u64) { self.bytes.fetch_add(count, Ordering::Relaxed); }
//! #     fn checkpoint(&self) -> Checkpoint {
//! #         if self.stop.load(Ordering::Relaxed) { Checkpoint::Cancel } else { Checkpoint::Continue }
//! #     }
//! # }
//! ```

/// What an operation should do at a point where stopping is safe.
///
/// Two states, not three, on purpose. A *paused* copy does not stop — it holds
/// still, and holding still is the reporter's business rather than the copy
/// loop's: an implementor answers [`Checkpoint::Continue`] until someone says
/// otherwise, and can block inside [`ProgressReporter::checkpoint`] for as long
/// as the pause lasts. The copy code then needs no notion of pausing at all,
/// and the only distinction it has to act on is "may I keep going".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checkpoint {
    /// Keep going.
    Continue,
    /// Stop where you are, and leave nothing half-finished behind.
    Cancel,
}

/// How a long operation reports itself, and learns that it should stop.
///
/// Called from blocking-pool threads, between units of work — never from a
/// frame, and never more than once per [`config::COPY_CHUNK_BYTES`] for a copy.
/// Every method is therefore expected to be cheap; the counter implementations
/// in this module are single relaxed atomics.
///
/// `checkpoint` is the one method allowed to take time, and only an implementor
/// that models pausing will: a cancellable copy calls it once per chunk, so a
/// pause that blocks there holds one worker thread rather than tearing down a
/// half-written file and having to start it again.
pub trait ProgressReporter: Send + Sync {
    /// Bytes written to, or read from, the destination since the last call.
    fn bytes(&self, count: u64) {
        let _ = count;
    }

    /// Files created or removed since the last call.
    ///
    /// Separate from [`ProgressReporter::bytes`] because a delete moves no
    /// bytes and a thousand-file delete is still real work: counting entries is
    /// what gives a tree delete something to show.
    fn entries(&self, count: u64) {
        let _ = count;
    }

    /// Asked between units of work whether to continue.
    fn checkpoint(&self) -> Checkpoint {
        Checkpoint::Continue
    }
}

use std::sync::atomic::{AtomicU64, Ordering};

/// The reporter used when nobody is listening.
///
/// Every counter is a real atomic rather than a discarded value, so the
/// uninstrumented path is the same code with the arithmetic left in. It costs
/// one relaxed add per chunk, which is nothing next to the write it follows,
/// and it means [`Vfs::copy`] and [`Vfs::copy_observed`] cannot drift apart.
#[derive(Debug, Default)]
pub struct Meters {
    bytes: AtomicU64,
    entries: AtomicU64,
}

impl Meters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }
}

impl ProgressReporter for Meters {
    fn bytes(&self, count: u64) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
    }

    fn entries(&self, count: u64) {
        self.entries.fetch_add(count, Ordering::Relaxed);
    }
}
