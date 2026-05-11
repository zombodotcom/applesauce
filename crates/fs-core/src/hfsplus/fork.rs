//! Fork data — the per-fork descriptor of where a file's bytes live.
//!
//! A file in HFS+ has up to two forks: data and resource. Each fork is
//! described by an `HFSPlusForkData` containing the fork's logical size
//! and its first eight extents. Files with more than eight extents
//! ("fragmented") spill into the extents overflow B-tree, which we
//! consult later when resolving file reads.

use binrw::BinRead;

/// (start_block, block_count) — a contiguous run of allocation blocks.
#[derive(BinRead, Debug, Clone, Copy, PartialEq, Eq)]
#[brw(big)]
pub struct HFSPlusExtentDescriptor {
    pub start_block: u32,
    pub block_count: u32,
}

impl HFSPlusExtentDescriptor {
    /// True if this slot is unused. HFS+ marks unused extents with
    /// block_count == 0.
    pub fn is_empty(&self) -> bool {
        self.block_count == 0
    }
}

/// 80-byte fork descriptor. The first 8 extents are inline; further
/// extents go to the extents overflow file.
#[derive(BinRead, Debug, Clone, Copy)]
#[brw(big)]
pub struct HFSPlusForkData {
    pub logical_size: u64,
    pub clump_size: u32,
    pub total_blocks: u32,
    pub extents: [HFSPlusExtentDescriptor; 8],
}

impl HFSPlusForkData {
    /// Sum of block_count across the inline extents. Useful to detect
    /// fragmentation (if this is less than total_blocks, the file has
    /// extents in the overflow B-tree).
    pub fn inline_blocks(&self) -> u32 {
        self.extents.iter().map(|e| e.block_count).sum()
    }

    /// True if the fork is fully described by the inline extents.
    pub fn is_fully_inline(&self) -> bool {
        self.inline_blocks() == self.total_blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use binrw::BinReaderExt;

    #[test]
    fn extent_descriptor_round_trip() {
        let bytes: [u8; 8] = [0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x07];
        let mut c = Cursor::new(&bytes);
        let d: HFSPlusExtentDescriptor = c.read_be().unwrap();
        assert_eq!(d.start_block, 5);
        assert_eq!(d.block_count, 7);
        assert!(!d.is_empty());
    }

    #[test]
    fn fork_data_parses_and_detects_inline() {
        let mut buf = vec![0u8; 80];
        // logical_size = 0x1234
        buf[0..8].copy_from_slice(&0x1234u64.to_be_bytes());
        // clump_size = 4096
        buf[8..12].copy_from_slice(&4096u32.to_be_bytes());
        // total_blocks = 3
        buf[12..16].copy_from_slice(&3u32.to_be_bytes());
        // extent[0] = (100, 3)
        buf[16..20].copy_from_slice(&100u32.to_be_bytes());
        buf[20..24].copy_from_slice(&3u32.to_be_bytes());
        // rest of extents zeroed

        let mut c = Cursor::new(&buf);
        let f: HFSPlusForkData = c.read_be().unwrap();
        assert_eq!(f.logical_size, 0x1234);
        assert_eq!(f.total_blocks, 3);
        assert_eq!(f.extents[0].start_block, 100);
        assert_eq!(f.extents[0].block_count, 3);
        assert!(f.extents[1].is_empty());
        assert_eq!(f.inline_blocks(), 3);
        assert!(f.is_fully_inline());
    }
}
