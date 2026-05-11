//! Image-file backed block source.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::BlockSource;

/// A block source backed by an on-disk image file.
pub struct ImageFile {
    file: File,
    len: u64,
}

impl ImageFile {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}

impl Read for ImageFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for ImageFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}

impl BlockSource for ImageFile {
    fn len_bytes(&self) -> Option<u64> {
        Some(self.len)
    }
}
