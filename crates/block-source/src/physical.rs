//! Windows physical-disk block source.
//!
//! Wraps `\\.\PhysicalDriveN` and similar device paths. Requires the
//! process to be elevated (Run as Administrator).
//!
//! NOTE: only stubs are present in this commit. Real implementation
//! follows in the block-source PhysicalDisk task.

use std::io::{Read, Seek, SeekFrom};

use crate::BlockSource;

/// Windows raw physical disk handle.
pub struct PhysicalDisk {
    // Holds the win32 file handle once implemented.
    _placeholder: (),
}

impl PhysicalDisk {
    /// Open `\\.\PhysicalDriveN`. Requires admin.
    pub fn open(_drive_number: u32) -> std::io::Result<Self> {
        unimplemented!("PhysicalDisk::open lands in the next commit")
    }
}

impl Read for PhysicalDisk {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        unimplemented!()
    }
}

impl Seek for PhysicalDisk {
    fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
        unimplemented!()
    }
}

impl BlockSource for PhysicalDisk {
    fn len_bytes(&self) -> Option<u64> {
        None
    }
}
