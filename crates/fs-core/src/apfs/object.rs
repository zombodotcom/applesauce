//! Block-level reads for APFS objects.
//!
//! APFS addresses storage in fixed-size blocks (`nx_block_size`,
//! usually 4096). Container-managed objects begin with an [`ObjPhys`]
//! header carrying a Fletcher-64 checksum we can use to reject garbage
//! blocks during the checkpoint scan.

use std::io::{Read, Seek, SeekFrom};

use anyhow::{bail, Result};
use binrw::BinReaderExt;

use super::types::{verify_checksum, ObjPhys, Paddr};

/// Reads APFS blocks from a source at a known block size.
pub struct BlockReader<'a, S> {
    source: &'a mut S,
    block_size: u32,
}

impl<'a, S: Read + Seek> BlockReader<'a, S> {
    pub fn new(source: &'a mut S, block_size: u32) -> Self {
        Self { source, block_size }
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Read the full block at physical address `paddr` into a buffer.
    pub fn read_block(&mut self, paddr: Paddr) -> Result<Vec<u8>> {
        if paddr < 0 {
            bail!("negative block address {paddr}");
        }
        let offset = paddr as u64 * self.block_size as u64;
        self.source.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; self.block_size as usize];
        self.source.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Read a block and parse its [`ObjPhys`] header, returning both.
    /// Does not verify the checksum (callers that scan untrusted areas
    /// should call [`block_checksum_ok`]).
    pub fn read_object(&mut self, paddr: Paddr) -> Result<(ObjPhys, Vec<u8>)> {
        let buf = self.read_block(paddr)?;
        let mut cursor = std::io::Cursor::new(&buf);
        let header: ObjPhys = cursor.read_le()?;
        Ok((header, buf))
    }
}

/// True if the block's stored Fletcher-64 checksum is self-consistent.
pub fn block_checksum_ok(block: &[u8]) -> bool {
    verify_checksum(block)
}
