//! APFS shared on-disk primitives: the object header that prefixes
//! every container-managed block, the magic numbers, object-type
//! masks, and the Fletcher-64 checksum APFS stamps into each object.
//!
//! Everything in APFS is little-endian (HFS+ was big-endian).

use binrw::BinRead;

/// `oid_t` — an object identifier. Physical oids are block addresses;
/// virtual oids are resolved to block addresses through an object map.
pub type Oid = u64;
/// `xid_t` — a transaction identifier. Higher = more recent.
pub type Xid = u64;
/// `paddr_t` — a physical block address. Signed on disk; negative
/// values are not used for the structures we read.
pub type Paddr = i64;

/// Container superblock magic: the bytes `NXSB` read little-endian.
pub const NX_MAGIC: u32 = 0x4253_584E;
/// Volume superblock magic: the bytes `APSB` read little-endian.
pub const APFS_MAGIC: u32 = 0x4253_5041;

/// `NX_MAX_FILE_SYSTEMS` — length of the `nx_fs_oid` array.
pub const NX_MAX_FILE_SYSTEMS: usize = 100;

/// Smallest plausible APFS block size; also the size of the prefix we
/// read to bootstrap the container superblock before we know the real
/// `nx_block_size`.
pub const NX_MINIMUM_BLOCK_SIZE: usize = 4096;

// --- object type (obj_phys_t.o_type) masks and values ----------------

/// Low 16 bits of `o_type` select the object's kind.
pub const OBJECT_TYPE_MASK: u32 = 0x0000_ffff;
/// High bits of `o_type` select the storage class.
pub const OBJ_STORAGETYPE_MASK: u32 = 0xc000_0000;
/// Virtual object: oid resolved through an object map.
pub const OBJ_VIRTUAL: u32 = 0x0000_0000;
/// Physical object: oid is the block address directly.
pub const OBJ_PHYSICAL: u32 = 0x4000_0000;
/// Ephemeral object: lives in the checkpoint data area.
pub const OBJ_EPHEMERAL: u32 = 0x8000_0000;

pub const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 0x0001;
pub const OBJECT_TYPE_BTREE: u32 = 0x0002;
pub const OBJECT_TYPE_BTREE_NODE: u32 = 0x0003;
pub const OBJECT_TYPE_OMAP: u32 = 0x000b;
pub const OBJECT_TYPE_CHECKPOINT_MAP: u32 = 0x000c;
pub const OBJECT_TYPE_FS: u32 = 0x000d;

/// The 32-byte header (`obj_phys_t`) at the start of every object that
/// the container manages (superblocks, omaps, B-tree nodes).
#[derive(BinRead, Debug, Clone)]
#[brw(little)]
pub struct ObjPhys {
    /// Fletcher-64 over the rest of the object (offset 8..end).
    pub cksum: [u8; 8],
    pub oid: Oid,
    pub xid: Xid,
    pub o_type: u32,
    pub subtype: u32,
}

impl ObjPhys {
    /// The object kind (low 16 bits of `o_type`).
    pub fn kind(&self) -> u32 {
        self.o_type & OBJECT_TYPE_MASK
    }

    /// The storage class bits of `o_type`.
    pub fn storage(&self) -> u32 {
        self.o_type & OBJ_STORAGETYPE_MASK
    }
}

/// APFS object checksum: a Fletcher-64 variant computed over the object
/// starting *after* the 8-byte checksum field, processing the data as
/// little-endian u32 words. Returns the 8-byte checksum that should be
/// stored in `obj_phys_t.o_cksum`.
///
/// Algorithm (per the Apple File System Reference / community notes):
/// accumulate two 32-bit-modulus sums `lower`/`upper` over each u32
/// word, then derive the two check words. `block` is the full object
/// block *including* the 8-byte checksum slot, which is treated as
/// zero for the computation.
pub fn fletcher64(block: &[u8]) -> [u8; 8] {
    // Work over the data after the first 8 bytes (the checksum slot).
    let data = &block[8..];
    let mut lower: u64 = 0;
    let mut upper: u64 = 0;
    const MOD: u64 = 0xffff_ffff;

    for chunk in data.chunks_exact(4) {
        let word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as u64;
        lower = (lower + word) % MOD;
        upper = (upper + lower) % MOD;
    }

    let check_low = MOD - ((lower + upper) % MOD);
    let check_high = MOD - ((lower + check_low) % MOD);
    let checksum = (check_high << 32) | check_low;
    checksum.to_le_bytes()
}

/// Verify the Fletcher-64 checksum stored in an object block. Returns
/// `true` if the stored checksum matches a recomputation.
pub fn verify_checksum(block: &[u8]) -> bool {
    if block.len() < 8 {
        return false;
    }
    let computed = fletcher64(block);
    computed == block[0..8]
}

#[cfg(test)]
mod tests {
    use super::*;
    use binrw::BinReaderExt;
    use std::io::Cursor;

    #[test]
    fn obj_header_parses_little_endian() {
        let mut buf = vec![0u8; 32];
        buf[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // cksum
        buf[8..16].copy_from_slice(&0x1122u64.to_le_bytes()); // oid
        buf[16..24].copy_from_slice(&0x33u64.to_le_bytes()); // xid
        buf[24..28].copy_from_slice(&(OBJ_PHYSICAL | OBJECT_TYPE_OMAP).to_le_bytes());
        buf[28..32].copy_from_slice(&0u32.to_le_bytes());

        let mut c = Cursor::new(buf);
        let h: ObjPhys = c.read_le().unwrap();
        assert_eq!(h.oid, 0x1122);
        assert_eq!(h.xid, 0x33);
        assert_eq!(h.kind(), OBJECT_TYPE_OMAP);
        assert_eq!(h.storage(), OBJ_PHYSICAL);
    }

    #[test]
    fn fletcher64_roundtrips() {
        // Build a block, stamp its checksum, and confirm verify passes.
        let mut block = vec![0u8; 4096];
        for (i, b) in block[8..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let sum = fletcher64(&block);
        block[0..8].copy_from_slice(&sum);
        assert!(verify_checksum(&block));

        // Corrupt a byte → verification fails.
        block[100] ^= 0xff;
        assert!(!verify_checksum(&block));
    }
}
