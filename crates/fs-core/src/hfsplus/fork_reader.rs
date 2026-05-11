//! Read a fork's logical bytes by resolving extents on demand.
//!
//! A `ForkReader` wraps a `HFSPlusForkData` plus the parent block
//! source and presents the fork as a `Read + Seek`. Internally it
//! maps the logical offset to (extent, in-extent offset) and reads
//! the appropriate range from the underlying source.
//!
//! For v0, only the eight inline extents are consulted. Files that
//! spill into the extents overflow B-tree return ErrorKind::Other
//! at the boundary — overflow handling lands together with the
//! extents-overflow B-tree work.

use std::io::{self, Read, Seek, SeekFrom};

use super::fork::HFSPlusForkData;

/// Reads a fork's logical content over an underlying block source.
pub struct ForkReader<'a, S> {
    source: &'a mut S,
    /// Byte offset of the start of the HFS+ volume in `source`.
    volume_offset: u64,
    /// HFS+ allocation block size (from the volume header).
    block_size: u32,
    fork: HFSPlusForkData,
    pos: u64,
}

impl<'a, S: Read + Seek> ForkReader<'a, S> {
    pub fn new(
        source: &'a mut S,
        volume_offset: u64,
        block_size: u32,
        fork: HFSPlusForkData,
    ) -> Self {
        Self {
            source,
            volume_offset,
            block_size,
            fork,
            pos: 0,
        }
    }

    /// Total logical size of the fork.
    pub fn len(&self) -> u64 {
        self.fork.logical_size
    }

    /// Translate a logical byte offset into a disk byte offset and the
    /// number of contiguous bytes available from there. Returns
    /// `Ok(None)` past EOF, and `Err` if the byte falls in an extent
    /// not represented in the inline list.
    fn resolve(&self, logical: u64) -> io::Result<Option<(u64, u64)>> {
        if logical >= self.fork.logical_size {
            return Ok(None);
        }

        let block_size = self.block_size as u64;
        let mut blocks_before: u64 = 0;

        for extent in &self.fork.extents {
            if extent.is_empty() {
                break;
            }
            let extent_start_logical = blocks_before * block_size;
            let extent_len_bytes = extent.block_count as u64 * block_size;
            let extent_end_logical = extent_start_logical + extent_len_bytes;

            if logical < extent_end_logical {
                let offset_in_extent = logical - extent_start_logical;
                let disk_offset = self.volume_offset
                    + extent.start_block as u64 * block_size
                    + offset_in_extent;
                let bytes_left_in_extent = extent_len_bytes - offset_in_extent;
                // Don't return more than the logical size.
                let bytes_to_eof = self.fork.logical_size - logical;
                let avail = bytes_left_in_extent.min(bytes_to_eof);
                return Ok(Some((disk_offset, avail)));
            }
            blocks_before += extent.block_count as u64;
        }

        // Logical offset is within logical_size but not covered by the
        // inline extents: must be in the extents overflow B-tree.
        Err(io::Error::new(
            io::ErrorKind::Other,
            "fork extent outside inline list (overflow B-tree not yet supported)",
        ))
    }
}

impl<'a, S: Read + Seek> Read for ForkReader<'a, S> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let Some((disk, avail)) = self.resolve(self.pos)? else {
            return Ok(0);
        };
        let take = (out.len() as u64).min(avail) as usize;
        self.source.seek(SeekFrom::Start(disk))?;
        let mut filled = 0;
        while filled < take {
            match self.source.read(&mut out[filled..take]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.pos += filled as u64;
        Ok(filled)
    }
}

impl<'a, S: Read + Seek> Seek for ForkReader<'a, S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::Current(o) => self.pos as i64 + o,
            SeekFrom::End(o) => self.fork.logical_size as i64 + o,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of fork",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hfsplus::fork::{HFSPlusExtentDescriptor, HFSPlusForkData};
    use std::io::Cursor;

    fn fork_with(extents: &[(u32, u32)], logical_size: u64) -> HFSPlusForkData {
        let mut e = [HFSPlusExtentDescriptor { start_block: 0, block_count: 0 }; 8];
        for (i, &(s, c)) in extents.iter().enumerate() {
            e[i] = HFSPlusExtentDescriptor { start_block: s, block_count: c };
        }
        let total_blocks: u32 = extents.iter().map(|&(_, c)| c).sum();
        HFSPlusForkData {
            logical_size,
            clump_size: 0,
            total_blocks,
            extents: e,
        }
    }

    #[test]
    fn reads_single_extent() {
        // Disk: 4 blocks of 16 bytes each, blocks 1..3 contain "hello world!!!" pattern
        let block_size = 16u32;
        let mut disk = vec![0u8; block_size as usize * 8];
        // Put bytes starting at block 2 (offset 32)
        let payload = b"hello world data";
        disk[32..48].copy_from_slice(payload);

        // Fork: extent at block 2, count 1, logical size 16
        let fork = fork_with(&[(2, 1)], 16);
        let mut src = Cursor::new(disk);
        let mut reader = ForkReader::new(&mut src, 0, block_size, fork);

        let mut out = vec![0u8; 16];
        reader.read_exact(&mut out).unwrap();
        assert_eq!(&out, payload);
    }

    #[test]
    fn reads_across_two_extents() {
        let block_size = 16u32;
        let mut disk = vec![0u8; block_size as usize * 16];
        // Extent 1 at block 1: "AAAAAAAAAAAAAAAA"
        disk[16..32].copy_from_slice(&[b'A'; 16]);
        // Extent 2 at block 5: "BBBBBBBBBBBBBBBB"
        disk[80..96].copy_from_slice(&[b'B'; 16]);

        let fork = fork_with(&[(1, 1), (5, 1)], 32);
        let mut src = Cursor::new(disk);
        let mut reader = ForkReader::new(&mut src, 0, block_size, fork);

        let mut out = vec![0u8; 32];
        // Loop because each .read() returns only up to the end of one extent
        let mut total = 0;
        while total < 32 {
            let n = reader.read(&mut out[total..]).unwrap();
            assert!(n > 0);
            total += n;
        }
        assert_eq!(&out[..16], &[b'A'; 16]);
        assert_eq!(&out[16..], &[b'B'; 16]);
    }

    #[test]
    fn respects_logical_size_smaller_than_extents() {
        // Fork claims one block (16 bytes of disk space) but logical_size is 10.
        let block_size = 16u32;
        let mut disk = vec![0u8; 32];
        disk[16..32].copy_from_slice(b"0123456789ABCDEF");

        let fork = fork_with(&[(1, 1)], 10);
        let mut src = Cursor::new(disk);
        let mut reader = ForkReader::new(&mut src, 0, block_size, fork);

        let mut out = vec![0u8; 32];
        let n = reader.read(&mut out).unwrap();
        assert_eq!(n, 10);
        assert_eq!(&out[..n], b"0123456789");
        // Next read returns EOF.
        assert_eq!(reader.read(&mut out).unwrap(), 0);
    }

    #[test]
    fn seek_then_read() {
        let block_size = 16u32;
        let mut disk = vec![0u8; 64];
        disk[0..16].copy_from_slice(b"first  block!!!\n");
        disk[16..32].copy_from_slice(b"second block!!!\n");

        let fork = fork_with(&[(0, 2)], 32);
        let mut src = Cursor::new(disk);
        let mut reader = ForkReader::new(&mut src, 0, block_size, fork);

        reader.seek(SeekFrom::Start(16)).unwrap();
        let mut out = vec![0u8; 16];
        reader.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"second block!!!\n");
    }

    #[test]
    fn volume_offset_applied() {
        // Fork resides inside a volume that starts at byte 100 of the disk.
        let block_size = 16u32;
        let mut disk = vec![0u8; 200];
        disk[100 + 16..100 + 32].copy_from_slice(b"payload at vol+1");

        let fork = fork_with(&[(1, 1)], 16);
        let mut src = Cursor::new(disk);
        let mut reader = ForkReader::new(&mut src, 100, block_size, fork);

        let mut out = vec![0u8; 16];
        reader.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"payload at vol+1");
    }
}
