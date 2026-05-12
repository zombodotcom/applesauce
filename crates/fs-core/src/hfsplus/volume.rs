//! HFS+ Volume Header parsing.
//!
//! Located at byte 1024 of the volume, 512 bytes long, all big-endian.
//! Identifies the volume and points at the locations of the four
//! special files: allocation bitmap, extents overflow, catalog, and
//! attributes B-tree.

use std::io::{Read, Seek, SeekFrom};

use anyhow::{anyhow, bail, Result};
use binrw::{BinRead, BinReaderExt};

use super::fork::HFSPlusForkData;
use super::types::{HFSPLUS_SIGNATURE, HFSX_SIGNATURE, HFS_SIGNATURE, VOLUME_HEADER_OFFSET};

#[derive(BinRead, Debug, Clone)]
#[brw(big)]
pub struct HFSPlusVolumeHeader {
    pub signature: u16,
    pub version: u16,
    pub attributes: u32,
    pub last_mounted_version: u32,
    pub journal_info_block: u32,

    pub create_date: u32,
    pub modify_date: u32,
    pub backup_date: u32,
    pub checked_date: u32,

    pub file_count: u32,
    pub folder_count: u32,

    pub block_size: u32,
    pub total_blocks: u32,
    pub free_blocks: u32,

    pub next_allocation: u32,
    pub rsrc_clump_size: u32,
    pub data_clump_size: u32,
    pub next_catalog_id: u32,
    pub write_count: u32,
    pub encodings_bitmap: u64,

    pub finder_info: [u32; 8],

    pub allocation_file: HFSPlusForkData,
    pub extents_file: HFSPlusForkData,
    pub catalog_file: HFSPlusForkData,
    pub attributes_file: HFSPlusForkData,
    pub startup_file: HFSPlusForkData,
}

impl HFSPlusVolumeHeader {
    /// Read and validate the volume header from a `Read + Seek` source
    /// positioned at the start of the HFS+ volume.
    pub fn read<S: Read + Seek>(source: &mut S) -> Result<Self> {
        Self::read_at(source, VOLUME_HEADER_OFFSET)
    }

    /// Read and validate the volume header at an absolute byte offset
    /// in the source. Used when the volume sits inside an HFS wrapper
    /// (see [`detect_hfs_wrapper`]).
    pub fn read_at<S: Read + Seek>(source: &mut S, byte_offset: u64) -> Result<Self> {
        source.seek(SeekFrom::Start(byte_offset))?;
        let header: HFSPlusVolumeHeader = source
            .read_be()
            .map_err(|e| anyhow!("decoding HFS+ volume header: {e}"))?;
        header.validate()?;
        Ok(header)
    }

    fn validate(&self) -> Result<()> {
        if self.signature != HFSPLUS_SIGNATURE && self.signature != HFSX_SIGNATURE {
            bail!(
                "not an HFS+ volume (signature 0x{:04X}, expected 0x{:04X} or 0x{:04X})",
                self.signature,
                HFSPLUS_SIGNATURE,
                HFSX_SIGNATURE
            );
        }
        // Version 4 = HFS+, 5 = HFSX.
        if self.version != 4 && self.version != 5 {
            bail!(
                "unsupported HFS+ volume version {} (expected 4 or 5)",
                self.version
            );
        }
        // block_size must be a power of two and at least 512.
        if self.block_size < 512 || !self.block_size.is_power_of_two() {
            bail!(
                "invalid HFS+ block size {}: must be a power of two >= 512",
                self.block_size
            );
        }
        Ok(())
    }

    /// True if this volume is HFSX (case-sensitive).
    pub fn is_case_sensitive(&self) -> bool {
        self.signature == HFSX_SIGNATURE
    }
}

/// Probe a source for an HFS-wrapped HFS+ volume (Mac OS 8.1–10.3
/// formatted disks). Returns the byte offset of the embedded HFS+
/// volume's start (relative to `volume_offset`), or `None` if the
/// signature at `volume_offset + 1024` is not HFS classic.
///
/// Errors out if it _is_ HFS classic but doesn't embed an HFS+
/// volume — pure classic HFS is not something this crate reads.
pub fn detect_hfs_wrapper<S: Read + Seek>(
    source: &mut S,
    volume_offset: u64,
) -> Result<Option<u64>> {
    source.seek(SeekFrom::Start(volume_offset + VOLUME_HEADER_OFFSET))?;
    let mut mdb = [0u8; 130];
    source.read_exact(&mut mdb)?;

    let sig = u16::from_be_bytes([mdb[0], mdb[1]]);
    if sig != HFS_SIGNATURE {
        return Ok(None);
    }

    let dr_al_blk_siz = u32::from_be_bytes([mdb[20], mdb[21], mdb[22], mdb[23]]);
    let dr_al_bl_st = u16::from_be_bytes([mdb[28], mdb[29]]);
    let dr_embed_sig = u16::from_be_bytes([mdb[124], mdb[125]]);
    let embed_start = u16::from_be_bytes([mdb[126], mdb[127]]);

    if dr_embed_sig != HFSPLUS_SIGNATURE && dr_embed_sig != HFSX_SIGNATURE {
        bail!(
            "classic HFS volume (signature 0x{HFS_SIGNATURE:04X}) with no embedded HFS+ \
             (drEmbedSigWord = 0x{dr_embed_sig:04X}); pure HFS reading is not supported"
        );
    }

    let embedded =
        volume_offset + dr_al_bl_st as u64 * 512 + embed_start as u64 * dr_al_blk_siz as u64;
    Ok(Some(embedded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a minimal valid volume header. Returns the full 1536-byte
    /// prefix (1024 boot blocks + 512 header).
    fn build_minimal(signature: u16, version: u16, block_size: u32) -> Vec<u8> {
        let mut disk = vec![0u8; 1024 + 512];
        let h = &mut disk[1024..1024 + 512];
        h[0..2].copy_from_slice(&signature.to_be_bytes());
        h[2..4].copy_from_slice(&version.to_be_bytes());
        // skip to block_size at offset 40 (signature=0,version=2,attributes=4,lmv=8,journal=12,
        // dates 16/20/24/28, fileCount=32, folderCount=36, blockSize=40)
        h[40..44].copy_from_slice(&block_size.to_be_bytes());
        // total_blocks = 1000
        h[44..48].copy_from_slice(&1000u32.to_be_bytes());
        // catalog file fork at offset 240 (5*4 + 4 fields*4 = 20; volume header offsets
        // are best documented inline) — for now, fields beyond block_size
        // are all zero, which is fine because validation only checks the
        // first few fields and the binrw struct reads forks regardless.
        disk
    }

    #[test]
    fn rejects_non_hfsplus_signature() {
        let disk = build_minimal(0xDEAD, 4, 4096);
        let mut c = Cursor::new(disk);
        let err = HFSPlusVolumeHeader::read(&mut c).unwrap_err();
        assert!(err.to_string().contains("not an HFS+ volume"));
    }

    #[test]
    fn rejects_bad_version() {
        let disk = build_minimal(HFSPLUS_SIGNATURE, 99, 4096);
        let mut c = Cursor::new(disk);
        let err = HFSPlusVolumeHeader::read(&mut c).unwrap_err();
        assert!(err.to_string().contains("unsupported HFS+ volume version"));
    }

    #[test]
    fn rejects_non_pow2_block_size() {
        let disk = build_minimal(HFSPLUS_SIGNATURE, 4, 4100);
        let mut c = Cursor::new(disk);
        let err = HFSPlusVolumeHeader::read(&mut c).unwrap_err();
        assert!(err.to_string().contains("invalid HFS+ block size"));
    }

    #[test]
    fn parses_minimal_valid_header() {
        let disk = build_minimal(HFSPLUS_SIGNATURE, 4, 4096);
        let mut c = Cursor::new(disk);
        let h = HFSPlusVolumeHeader::read(&mut c).unwrap();
        assert_eq!(h.signature, HFSPLUS_SIGNATURE);
        assert_eq!(h.version, 4);
        assert_eq!(h.block_size, 4096);
        assert_eq!(h.total_blocks, 1000);
        assert!(!h.is_case_sensitive());
    }

    #[test]
    fn parses_hfsx_signature() {
        let disk = build_minimal(HFSX_SIGNATURE, 5, 4096);
        let mut c = Cursor::new(disk);
        let h = HFSPlusVolumeHeader::read(&mut c).unwrap();
        assert!(h.is_case_sensitive());
    }
}
