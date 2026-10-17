//! Ferret's storage layer.
//!
//! A thin, honest wrapper over what the platform already provides — not a
//! filesystem reimplementation. Its whole job is to keep two promises that are
//! expensive to add after the fact: **nothing blocks a frame**, and **listings
//! stream**.
//!
//! ```no_run
//! use ferret_vfs::{ListingDetail, LocalFs, Vfs, VfsPath};
//! use futures::StreamExt;
//!
//! # fn main() {
//! smol::block_on(async {
//!     let fs = LocalFs::new();
//!     let mut entries = fs.read_dir(&VfsPath::local("/tmp"), ListingDetail::Full);
//!     while let Some(batch) = entries.next().await {
//!         if let Ok(batch) = batch {
//!             for entry in batch {
//!                 println!("{}", entry.name());
//!             }
//!         }
//!     }
//! });
//! # }
//! ```
//!
//! Layout follows the repository conventions in `AGENTS.md`: the error type and
//! the crate's [`Result`] alias live in `errors.rs`, and every tunable lives in
//! [`config`].
//!
//! View preferences are *not* here — they are a settings concern and live in
//! `ferret-settings`. This crate deals only with storage.
//!
//! Shared domain types live here for now. They move to a `ferret-core` crate
//! when a second service crate needs them — not before.

pub mod config;
pub mod pool;
pub mod watch;

mod entry;
mod errors;
mod local;
mod path;
mod progress;
mod router;
mod vfs;

pub use entry::{Entry, EntryKind, Metadata, SymlinkTarget};
pub use errors::{Result, VfsError};
pub use local::LocalFs;
pub use path::{MountId, VfsPath, validate_portable_file_name};
pub use progress::{Checkpoint, Meters, ProgressReporter};
pub use router::Router;
pub use vfs::{
    Capabilities, DirEntryBatch, FileIdentity, ListingDetail, ReadDirStream, RemoveMode,
    SymlinkKind, Vfs, content_hash,
};
pub use watch::{Watch, WatchEvent};
