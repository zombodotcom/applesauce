//! APFS B-tree node parsing.
//!
//! A `btree_node_phys_t` is a 4 KiB-ish block: the 32-byte object
//! header, a small fixed node header, then a table of contents (TOC),
//! the keys area growing forward, free space, and the values area
//! growing backward from the end. The root node carries a 40-byte
//! `btree_info_t` trailer at the very end of the block.
//!
//! This module handles **fixed key/value-size** nodes (the
//! `BTNODE_FIXED_KV_SIZE` flag), which is what object maps use. The
//! variable-size layout used by the file-system tree lands in M2.

use binrw::BinRead;

use super::types::ObjPhys;

/// Offset of the node data area (TOC/keys/values) within the block:
/// `sizeof(obj_phys_t)=32 + flags/level/nkeys=8 + 4×nloc=16`.
pub const BTREE_NODE_DATA_OFFSET: usize = 56;
/// Size of the `btree_info_t` trailer present only on the root node.
pub const BTREE_INFO_SIZE: usize = 40;

pub const BTNODE_ROOT: u16 = 0x0001;
pub const BTNODE_LEAF: u16 = 0x0002;
pub const BTNODE_FIXED_KV_SIZE: u16 = 0x0004;

/// `nloc_t` — an (offset, length) location within the node data area.
#[derive(BinRead, Debug, Clone, Copy)]
#[brw(little)]
pub struct Nloc {
    pub off: u16,
    pub len: u16,
}

/// Fixed part of `btree_node_phys_t` (everything before the data area).
#[derive(BinRead, Debug, Clone)]
#[brw(little)]
pub struct BTreeNodeHeader {
    pub o: ObjPhys,
    pub flags: u16,
    pub level: u16,
    pub nkeys: u32,
    pub table_space: Nloc,
    pub free_space: Nloc,
    pub key_free_list: Nloc,
    pub val_free_list: Nloc,
}

impl BTreeNodeHeader {
    pub fn is_leaf(&self) -> bool {
        self.flags & BTNODE_LEAF != 0
    }
    pub fn is_root(&self) -> bool {
        self.flags & BTNODE_ROOT != 0
    }
    pub fn is_fixed_kv(&self) -> bool {
        self.flags & BTNODE_FIXED_KV_SIZE != 0
    }
}

/// A parsed fixed-KV B-tree node: the header plus the raw block, with
/// accessors that resolve TOC entries into key/value byte slices.
pub struct FixedKvNode {
    pub header: BTreeNodeHeader,
    block: Vec<u8>,
}

impl FixedKvNode {
    pub fn parse(block: Vec<u8>) -> anyhow::Result<Self> {
        use binrw::BinReaderExt;
        let mut cursor = std::io::Cursor::new(&block);
        let header: BTreeNodeHeader = cursor.read_le()?;
        Ok(Self { header, block })
    }

    pub fn nkeys(&self) -> usize {
        self.header.nkeys as usize
    }

    fn toc_start(&self) -> usize {
        BTREE_NODE_DATA_OFFSET + self.header.table_space.off as usize
    }

    fn key_area_start(&self) -> usize {
        BTREE_NODE_DATA_OFFSET
            + self.header.table_space.off as usize
            + self.header.table_space.len as usize
    }

    /// The end of the value area: the block end, minus the info
    /// trailer if this is the root node.
    fn value_area_end(&self) -> usize {
        if self.header.is_root() {
            self.block.len() - BTREE_INFO_SIZE
        } else {
            self.block.len()
        }
    }

    /// The `(key_off, val_off)` TOC entry `i` for a fixed-KV node.
    fn toc_entry(&self, i: usize) -> Option<(u16, u16)> {
        let off = self.toc_start() + i * 4;
        let k = u16::from_le_bytes([*self.block.get(off)?, *self.block.get(off + 1)?]);
        let v = u16::from_le_bytes([*self.block.get(off + 2)?, *self.block.get(off + 3)?]);
        Some((k, v))
    }

    /// Key bytes for entry `i`, given the fixed key size.
    pub fn key(&self, i: usize, key_size: usize) -> Option<&[u8]> {
        let (k, _) = self.toc_entry(i)?;
        let start = self.key_area_start() + k as usize;
        self.block.get(start..start + key_size)
    }

    /// Value bytes for entry `i`, given the fixed value size.
    pub fn value(&self, i: usize, val_size: usize) -> Option<&[u8]> {
        let (_, v) = self.toc_entry(i)?;
        let start = self.value_area_end().checked_sub(v as usize)?;
        self.block.get(start..start + val_size)
    }
}
