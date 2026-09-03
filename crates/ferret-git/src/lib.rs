//! Ferret's Git integration: repository discovery, status, and the four
//! operations the status bar offers.
//!
//! Git is reached through the **git binary**, not through libgit2 or gitoxide
//! (D-04, restated with its consequences in `docs/git.html` §2). The reason is
//! not performance: it is that the user's credential helper, SSH agent, 2FA,
//! commit signing, hooks and `includeIf` configuration all keep working, and
//! that what Ferret reports is by construction what `git status` prints in the
//! terminal panel six pixels below it. The costs — a runtime dependency on git
//! being installed, a process per query, and a wire format to parse — are paid
//! in `command.rs`, `config.rs` and `status.rs` respectively.
//!
//! This crate is GPUI-free and knows nothing about views. Every function that
//! spawns a process blocks; callers reach them from `ferret-ui` on the
//! background executor, never from a frame.

mod command;
mod discover;
mod errors;
mod ops;
mod status;

pub mod config;

pub use discover::{Repository, discover};
pub use errors::{GitError, Result};
pub use ops::{commit, pull, push, stage_all, status};
pub use status::{Counts, FileStatus, Head, RepoStatus, StatusIndex, Tracking};
