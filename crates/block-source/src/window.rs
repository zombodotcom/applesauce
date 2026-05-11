//! A `Window` exposes a byte range of an inner `BlockSource` as a
//! standalone `BlockSource`. Partition parsers return windows over
//! the parent disk; filesystem code then sees just its volume's bytes
//! starting at offset 0.

use std::io::{self, Read, Seek, SeekFrom};

use crate::BlockSource;

/// A sub-range view over an inner block source.
pub struct Window<S: BlockSource> {
    inner: S,
    start: u64,
    length: u64,
    pos: u64,
}

impl<S: BlockSource> Window<S> {
    /// Create a window of `length` bytes starting at `start` in `inner`.
    pub fn new(mut inner: S, start: u64, length: u64) -> io::Result<Self> {
        inner.seek(SeekFrom::Start(start))?;
        Ok(Self {
            inner,
            start,
            length,
            pos: 0,
        })
    }
}

impl<S: BlockSource> Read for Window<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.length.saturating_sub(self.pos);
        if remaining == 0 {
            return Ok(0);
        }
        let cap = buf.len().min(remaining as usize);
        // Ensure underlying position is correct (defensive — other code
        // may have seeked the inner source).
        self.inner.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.inner.read(&mut buf[..cap])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<S: BlockSource> Seek for Window<S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::Current(o) => self.pos as i64 + o,
            SeekFrom::End(o) => self.length as i64 + o,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Window seek before start",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

impl<S: BlockSource> BlockSource for Window<S> {
    fn len_bytes(&self) -> Option<u64> {
        Some(self.length)
    }
}
