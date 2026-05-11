//! Block-level access to disks and image files.
//!
//! Provides a unified `Read + Seek` view over Windows physical disks
//! (`\\.\PhysicalDriveN`) and on-disk image files (`.dmg`, `.img`, raw).
//! Filesystem code consumes a `BlockSource` and is decoupled from where
//! the bytes physically live.

#![deny(rust_2018_idioms)]

pub mod image;
pub mod partition;
pub mod window;

#[cfg(windows)]
pub mod physical;

use std::io::{Read, Seek};

/// Anything that can be addressed as a byte-oriented Read + Seek source.
///
/// Implementations must report the total size in bytes (where known) so
/// partition parsers can validate offsets.
pub trait BlockSource: Read + Seek + Send {
    /// Total size of the underlying medium in bytes, if known.
    fn len_bytes(&self) -> Option<u64>;
}
