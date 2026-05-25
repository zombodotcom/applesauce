//! APFS container superblock (`nx_superblock_t`) and the checkpoint
//! scan that finds the *current* one.
//!
//! Block 0 holds a valid container superblock, but it can be stale.
//! The live superblock is the highest-`xid` `NXSB` object in the
//! checkpoint descriptor area (`nx_xp_desc_base .. +nx_xp_desc_blocks`).
//! We read block 0 to learn the block size and descriptor-area
//! location, then scan that ring for the newest valid superblock.

use std::io::{Read, Seek};

use anyhow::{anyhow, bail, Result};
use binrw::{BinRead, BinReaderExt};

use super::filesystem::ApfsVolume;
use super::object::{block_checksum_ok, BlockReader};
use super::omap::Omap;
use super::types::{
    Oid, Paddr, Xid, NX_MAGIC, NX_MAX_FILE_SYSTEMS, NX_MINIMUM_BLOCK_SIZE,
    OBJECT_TYPE_NX_SUPERBLOCK,
};
use super::volume::ApfsVolumeInfo;

/// High bit of `nx_xp_desc_blocks` marks a tree-stored descriptor area
/// (rather than a contiguous run). We don't yet support the tree form.
const NX_XP_DESC_TREE_FLAG: u32 = 0x8000_0000;

/// `nx_superblock_t`, parsed through the `nx_fs_oid` array.
#[derive(BinRead, Debug, Clone)]
#[brw(little)]
pub struct NxSuperblock {
    #[br(pad_before = 32)] // obj_phys_t header
    pub magic: u32,
    pub block_size: u32,
    pub block_count: u64,
    pub features: u64,
    pub readonly_compatible_features: u64,
    pub incompatible_features: u64,
    pub uuid: [u8; 16],
    pub next_oid: Oid,
    pub next_xid: Xid,
    pub xp_desc_blocks: u32,
    pub xp_data_blocks: u32,
    pub xp_desc_base: Paddr,
    pub xp_data_base: Paddr,
    pub xp_desc_next: u32,
    pub xp_data_next: u32,
    pub xp_desc_index: u32,
    pub xp_desc_len: u32,
    pub xp_data_index: u32,
    pub xp_data_len: u32,
    pub spaceman_oid: Oid,
    pub omap_oid: Oid,
    pub reaper_oid: Oid,
    pub test_type: u32,
    pub max_file_systems: u32,
    #[br(count = NX_MAX_FILE_SYSTEMS)]
    pub fs_oid: Vec<Oid>,
}

impl NxSuperblock {
    fn validate(&self) -> Result<()> {
        if self.magic != NX_MAGIC {
            bail!(
                "not an APFS container (magic 0x{:08X}, expected 0x{:08X})",
                self.magic,
                NX_MAGIC
            );
        }
        if !(512..=65536).contains(&self.block_size) || !self.block_size.is_power_of_two() {
            bail!("implausible APFS block size {}", self.block_size);
        }
        Ok(())
    }
}

/// An opened APFS container: owns the source, knows its block size and
/// live superblock, and can enumerate the volumes inside it.
pub struct ApfsContainer<S> {
    source: S,
    block_size: u32,
    superblock: NxSuperblock,
}

impl<S: Read + Seek> ApfsContainer<S> {
    /// Open the container at the start of `source` (offset 0). Reads
    /// block 0, then scans the checkpoint descriptor ring for the
    /// newest valid `NXSB`.
    pub fn open(mut source: S) -> Result<Self> {
        // Bootstrap: read a minimum-size prefix as block 0 to learn the
        // real block size and descriptor-area location.
        let block0 = {
            use std::io::SeekFrom;
            source.seek(SeekFrom::Start(0))?;
            let mut buf = vec![0u8; NX_MINIMUM_BLOCK_SIZE];
            source.read_exact(&mut buf)?;
            buf
        };
        let mut cursor = std::io::Cursor::new(&block0);
        let base: NxSuperblock = cursor
            .read_le()
            .map_err(|e| anyhow!("decoding APFS container superblock: {e}"))?;
        base.validate()?;

        let block_size = base.block_size;
        let superblock = Self::find_live_superblock(&mut source, &base, block_size)?;

        Ok(Self {
            source,
            block_size,
            superblock,
        })
    }

    /// Scan the checkpoint descriptor ring for the `NXSB` with the
    /// highest xid. Falls back to the block-0 copy if the scan turns up
    /// nothing better (e.g. a freshly created container).
    fn find_live_superblock(
        source: &mut S,
        base: &NxSuperblock,
        block_size: u32,
    ) -> Result<NxSuperblock> {
        let desc_blocks = base.xp_desc_blocks;
        if desc_blocks & NX_XP_DESC_TREE_FLAG != 0 {
            // Tree-stored descriptor area is rare; the block-0 copy is
            // still a valid (if possibly older) superblock to fall back on.
            tracing::warn!("APFS checkpoint descriptor area is tree-stored; using block 0");
            return Ok(base.clone());
        }

        let mut reader = BlockReader::new(source, block_size);
        // Track the best candidate by its object-header xid. Seed with
        // the block-0 copy so a fruitless scan still yields a superblock.
        let mut best_xid: Xid = 0;
        let mut best: Option<NxSuperblock> = None;

        for i in 0..desc_blocks as i64 {
            let paddr = base.xp_desc_base + i;
            let block = match reader.read_block(paddr) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if !block_checksum_ok(&block) {
                continue;
            }
            // Only consider blocks whose header advertises a container
            // superblock, then confirm the embedded magic.
            let o_type = u32::from_le_bytes([block[24], block[25], block[26], block[27]]);
            if o_type & 0xffff != OBJECT_TYPE_NX_SUPERBLOCK {
                continue;
            }
            let mut c = std::io::Cursor::new(&block);
            let candidate: NxSuperblock = match c.read_le() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if candidate.magic != NX_MAGIC {
                continue;
            }
            let xid = u64::from_le_bytes(block[16..24].try_into().unwrap());
            if best.is_none() || xid > best_xid {
                best_xid = xid;
                best = Some(candidate);
            }
        }

        Ok(best.unwrap_or_else(|| base.clone()))
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn superblock(&self) -> &NxSuperblock {
        &self.superblock
    }

    /// Enumerate the volumes in the container: resolve each non-zero
    /// `nx_fs_oid` (a virtual oid) through the container OMAP to its
    /// `apfs_superblock_t` block, then parse that volume's metadata.
    pub fn volumes(&mut self) -> Result<Vec<ApfsVolumeInfo>> {
        let omap_paddr = self.superblock.omap_oid as Paddr;
        let block_size = self.block_size;
        let fs_oids: Vec<Oid> = self
            .superblock
            .fs_oid
            .iter()
            .copied()
            .filter(|&o| o != 0)
            .collect();

        let mut reader = BlockReader::new(&mut self.source, block_size);
        let omap = Omap::open(&mut reader, omap_paddr)?;

        let mut out = Vec::new();
        for oid in fs_oids {
            let Some(entry) = omap.lookup(&mut reader, oid, Xid::MAX)? else {
                tracing::warn!("APFS volume oid {oid} not found in container omap; skipping");
                continue;
            };
            match ApfsVolumeInfo::read(&mut reader, entry.paddr, oid) {
                Ok(info) => out.push(info),
                Err(e) => tracing::warn!("APFS volume oid {oid} superblock parse failed: {e:#}"),
            }
        }
        Ok(out)
    }

    /// Open one volume as a [`MacFilesystem`], consuming the container's
    /// source. (Each mounted/browsed volume otherwise wants its own
    /// source handle — the caller re-opens the disk per volume.)
    pub fn open_volume(self, info: &ApfsVolumeInfo) -> Result<ApfsVolume<S>>
    where
        S: Send,
    {
        ApfsVolume::open(
            self.source,
            self.block_size,
            info.omap_oid as Paddr,
            info.root_tree_oid,
            info.xid,
            info.hashed_drec_keys,
            info.case_insensitive,
            info.name.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apfs::btree::{BTNODE_FIXED_KV_SIZE, BTNODE_LEAF, BTNODE_ROOT};
    use crate::apfs::types::{fletcher64, APFS_MAGIC, OBJ_EPHEMERAL};
    use std::io::Cursor;

    const BS: usize = 4096;

    fn put_u16(b: &mut [u8], off: usize, v: u16) {
        b[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(b: &mut [u8], off: usize, v: u64) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn put_i64(b: &mut [u8], off: usize, v: i64) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Write `block` at block index `idx` into the disk image.
    fn place(disk: &mut [u8], idx: usize, block: &[u8]) {
        disk[idx * BS..idx * BS + BS].copy_from_slice(block);
    }

    /// Stamp a valid Fletcher-64 checksum into block[0..8].
    fn stamp(block: &mut [u8]) {
        let sum = fletcher64(block);
        block[0..8].copy_from_slice(&sum);
    }

    fn build_nxsb(xid: u64) -> Vec<u8> {
        let mut b = vec![0u8; BS];
        // obj header
        put_u64(&mut b, 16, xid); // o_xid
        put_u32(&mut b, 24, OBJ_EPHEMERAL | OBJECT_TYPE_NX_SUPERBLOCK); // o_type
                                                                        // body
        put_u32(&mut b, 32, NX_MAGIC);
        put_u32(&mut b, 36, BS as u32); // block_size
        put_u32(&mut b, 104, 4); // xp_desc_blocks
        put_i64(&mut b, 112, 1); // xp_desc_base = block 1
        put_u64(&mut b, 160, 8); // omap_oid = block 8 (physical)
        put_u32(&mut b, 180, 1); // max_file_systems
        put_u64(&mut b, 184, 300); // fs_oid[0] = virtual oid 300
        b
    }

    fn build_omap_phys() -> Vec<u8> {
        let mut b = vec![0u8; BS];
        put_u64(&mut b, 48, 9); // om_tree_oid = block 9
        b
    }

    fn build_omap_btree_leaf() -> Vec<u8> {
        let mut b = vec![0u8; BS];
        // node header (after 32-byte obj header)
        put_u16(&mut b, 32, BTNODE_ROOT | BTNODE_LEAF | BTNODE_FIXED_KV_SIZE);
        put_u16(&mut b, 34, 0); // level (leaf)
        put_u32(&mut b, 36, 1); // nkeys
        put_u16(&mut b, 40, 0); // table_space.off
        put_u16(&mut b, 42, 8); // table_space.len
                                // TOC at 56: entry { k=0, v=16 }
        put_u16(&mut b, 56, 0); // key offset within key area
        put_u16(&mut b, 58, 16); // value offset from value-area end
                                 // key area starts at 56 + off(0) + len(8) = 64
        put_u64(&mut b, 64, 300); // omap_key.oid
        put_u64(&mut b, 72, 1); // omap_key.xid
                                // value at value_area_end - v = (BS-40) - 16 = 4040
        let val = BS - BTREE_INFO_SIZE_TEST - 16;
        put_u32(&mut b, val, 0); // ov_flags
        put_u32(&mut b, val + 4, BS as u32); // ov_size
        put_i64(&mut b, val + 8, 10); // ov_paddr = block 10
        b
    }
    const BTREE_INFO_SIZE_TEST: usize = 40;

    fn build_apsb(name: &str, encrypted: bool) -> Vec<u8> {
        let mut b = vec![0u8; BS];
        put_u32(&mut b, 32, APFS_MAGIC);
        put_u64(&mut b, 88, 100); // fs_alloc_count
        put_u64(&mut b, 128, 0); // omap_oid (unused in M1)
        put_u64(&mut b, 136, 0); // root_tree_oid (unused in M1)
        put_u64(&mut b, 184, 42); // num_files
        put_u64(&mut b, 192, 7); // num_directories
        put_u64(&mut b, 264, if encrypted { 0 } else { 1 }); // fs_flags: bit0 = UNENCRYPTED
        put_u16(&mut b, 964, 0x0040); // role = Data
        let name_bytes = name.as_bytes();
        b[704..704 + name_bytes.len()].copy_from_slice(name_bytes);
        b
    }

    #[test]
    fn enumerates_one_volume_through_checkpoint_and_omap() {
        let mut disk = vec![0u8; BS * 16];

        // block 0: stale NXSB (xid 1), bootstrap source of block size +
        // descriptor base. Read raw, no checksum needed.
        place(&mut disk, 0, &build_nxsb(1));

        // checkpoint descriptor ring is blocks 1..=4. Put the live NXSB
        // (xid 10) at block 2 with a valid checksum; the scan must pick it.
        let mut live = build_nxsb(10);
        stamp(&mut live);
        place(&mut disk, 2, &live);

        place(&mut disk, 8, &build_omap_phys());
        place(&mut disk, 9, &build_omap_btree_leaf());
        place(&mut disk, 10, &build_apsb("Macintosh HD", false));

        let mut container = ApfsContainer::open(Cursor::new(disk)).unwrap();
        assert_eq!(container.block_size(), BS as u32);
        // The live superblock (xid 10) must have been selected; it carries
        // fs_oid[0] = 300.
        assert_eq!(container.superblock().fs_oid[0], 300);

        let vols = container.volumes().unwrap();
        assert_eq!(vols.len(), 1);
        let v = &vols[0];
        assert_eq!(v.name, "Macintosh HD");
        assert_eq!(v.num_files, 42);
        assert_eq!(v.num_directories, 7);
        assert_eq!(v.paddr, 10);
        assert!(!v.encrypted);
        assert_eq!(v.role_name(), "Data");
    }

    #[test]
    fn rejects_non_apfs_source() {
        let disk = vec![0u8; BS * 4];
        let err = match ApfsContainer::open(Cursor::new(disk)) {
            Ok(_) => panic!("expected failure on non-APFS source"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("not an APFS container"));
    }
}
