//! Mac filesystem reader trait and implementations.
//!
//! Provides a unified read-only interface over HFS+ and APFS volumes.
//! Filesystem implementations consume a `block_source::BlockSource`
//! and expose directories, file metadata, and ranged file reads.

#![deny(rust_2018_idioms)]

pub mod apfs;
pub mod hfsplus;
pub mod pull;

use std::time::SystemTime;

/// File-or-directory metadata.
#[derive(Debug, Clone)]
pub struct Stat {
    pub name: String,
    pub size_bytes: u64,
    pub is_dir: bool,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

/// A single entry returned by directory listing.
pub type DirEntry = Stat;

/// Read-only Mac filesystem.
pub trait MacFilesystem: Send {
    /// List the entries in the directory at the given POSIX-style path.
    fn list_dir(&mut self, path: &str) -> anyhow::Result<Vec<DirEntry>>;

    /// Stat a single path (file or directory).
    fn stat(&mut self, path: &str) -> anyhow::Result<Stat>;

    /// Read a range of bytes from a file.
    fn read_file_range(&mut self, path: &str, offset: u64, buf: &mut [u8])
        -> anyhow::Result<usize>;

    /// Human-readable volume label, if available.
    fn volume_label(&self) -> Option<&str>;
}
