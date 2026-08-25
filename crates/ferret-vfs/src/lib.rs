//! Ferret's storage layer.
//!
//! A thin, honest wrapper over what the platform already provides — not a
//! filesystem reimplementation. Its whole job is to keep two promises that are
//! expensive to add after the fact: **nothing blocks a frame**, and **listings
//! stream**.
//!
//! ```no_run
//! use ferret_vfs::{LocalFs, Vfs, VfsPath};
//! use futures::StreamExt;
//!
//! # fn main() {
//! smol::block_on(async {
//!     let fs = LocalFs::new();
//!     let mut entries = fs.read_dir(&VfsPath::local("/tmp"));
//!     while let Some(entry) = entries.next().await {
//!         if let Ok(entry) = entry {
//!             println!("{}", entry.name);
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
//! Shared domain types live here for now. They move to a `ferret-core` crate
//! when a second service crate needs them — not before.

pub mod config;

mod entry;
mod errors;
mod folder_prefs;
mod local;
mod path;
mod vfs;

pub use entry::{Entry, EntryKind, Metadata, SymlinkTarget};
pub use errors::{Result, VfsError};
pub use folder_prefs::{FolderPrefs, FolderView, SortKey, SortOrder, ViewMode};
pub use local::LocalFs;
pub use path::{MountId, VfsPath};
pub use vfs::{Capabilities, RemoveMode, Vfs};
