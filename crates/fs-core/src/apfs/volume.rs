//! APFS volume superblock (`apfs_superblock_t`, magic `APSB`).
//!
//! Carries the volume's name, role, object counts, allocation size,
//! feature/encryption flags, and the oids of its object map and
//! file-system (root) B-tree. We extract fields by their documented
//! byte offsets rather than modelling the whole struct (it has several
//! fixed arrays we don't need yet).

use std::io::{Read, Seek};

use anyhow::{bail, Result};

use super::object::BlockReader;
use super::types::{Oid, Paddr, Xid, APFS_MAGIC};

// Field offsets within apfs_superblock_t (after the 32-byte obj header).
const OFF_MAGIC: usize = 32;
const OFF_INCOMPAT_FEATURES: usize = 56;
const OFF_FS_ALLOC_COUNT: usize = 88;
const OFF_OMAP_OID: usize = 128;
const OFF_ROOT_TREE_OID: usize = 136;
const OFF_NUM_FILES: usize = 184;
const OFF_NUM_DIRS: usize = 192;
const OFF_FS_FLAGS: usize = 264;
const OFF_VOLNAME: usize = 704;
const OFF_ROLE: usize = 964;
const VOLNAME_LEN: usize = 256;

/// `apfs_fs_flags` bit: when set the volume is *not* encrypted.
/// Encrypted volumes have this clear.
const APFS_FS_UNENCRYPTED: u64 = 0x0000_0001;

/// `apfs_incompatible_features` bit: case-insensitive comparison.
const APFS_INCOMPAT_CASE_INSENSITIVE: u64 = 0x0000_0001;
/// `apfs_incompatible_features` bit: normalization-insensitive names.
const APFS_INCOMPAT_NORMALIZATION_INSENSITIVE: u64 = 0x0000_0008;

/// Parsed, human-facing metadata for one APFS volume.
#[derive(Debug, Clone)]
pub struct ApfsVolumeInfo {
    /// Virtual oid of this volume's superblock (its `nx_fs_oid` entry).
    pub fs_oid: Oid,
    /// Physical block where the superblock was found.
    pub paddr: Paddr,
    /// Transaction id (`o_xid`) of this superblock. Used as the ceiling
    /// when resolving fs-tree objects through the volume object map, so
    /// we read the snapshot consistent with this superblock rather than
    /// a newer (possibly uncommitted) transaction's objects.
    pub xid: Xid,
    /// Volume name (UTF-8, decoded from `apfs_volname`).
    pub name: String,
    /// `apfs_role` raw value.
    pub role: u16,
    /// Blocks currently allocated to the volume.
    pub alloc_count: u64,
    pub num_files: u64,
    pub num_directories: u64,
    /// True if the volume is FileVault-encrypted (file contents are
    /// ciphertext without the key).
    pub encrypted: bool,
    /// True if name comparison is case-insensitive.
    pub case_insensitive: bool,
    /// True if directory-entry keys use the hashed form
    /// (`j_drec_hashed_key_t`) — the case- or normalization-insensitive
    /// volumes do; case-sensitive ones use a bare name length.
    pub hashed_drec_keys: bool,
    /// Object map oid (for file-system walking).
    pub omap_oid: Oid,
    /// File-system (root) B-tree oid (virtual; resolved via the volume
    /// object map).
    pub root_tree_oid: Oid,
}

impl ApfsVolumeInfo {
    /// Read and parse the volume superblock at physical block `paddr`.
    pub fn read<S: Read + Seek>(
        reader: &mut BlockReader<'_, S>,
        paddr: Paddr,
        fs_oid: Oid,
    ) -> Result<Self> {
        let block = reader.read_block(paddr)?;
        let magic = read_u32(&block, OFF_MAGIC)?;
        if magic != APFS_MAGIC {
            bail!(
                "not an APFS volume superblock (magic 0x{:08X}, expected 0x{:08X})",
                magic,
                APFS_MAGIC
            );
        }

        let incompat = read_u64(&block, OFF_INCOMPAT_FEATURES)?;
        let fs_flags = read_u64(&block, OFF_FS_FLAGS)?;
        let name = read_cstr(&block, OFF_VOLNAME, VOLNAME_LEN)?;

        // o_xid lives at offset 16 of the obj_phys_t header.
        let xid = read_u64(&block, 16)?;

        Ok(Self {
            fs_oid,
            paddr,
            xid,
            name,
            role: read_u16(&block, OFF_ROLE)?,
            alloc_count: read_u64(&block, OFF_FS_ALLOC_COUNT)?,
            num_files: read_u64(&block, OFF_NUM_FILES)?,
            num_directories: read_u64(&block, OFF_NUM_DIRS)?,
            encrypted: fs_flags & APFS_FS_UNENCRYPTED == 0,
            case_insensitive: incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0,
            hashed_drec_keys: incompat
                & (APFS_INCOMPAT_CASE_INSENSITIVE | APFS_INCOMPAT_NORMALIZATION_INSENSITIVE)
                != 0,
            omap_oid: read_u64(&block, OFF_OMAP_OID)?,
            root_tree_oid: read_u64(&block, OFF_ROOT_TREE_OID)?,
        })
    }

    /// A short human label for the volume's role.
    pub fn role_name(&self) -> &'static str {
        match self.role {
            0x0000 => "none",
            0x0001 => "System",
            0x0002 => "User",
            0x0004 => "Recovery",
            0x0008 => "VM",
            0x0010 => "Preboot",
            0x0020 => "Installer",
            0x0040 => "Data",
            0x0080 => "Baseband",
            _ => "other",
        }
    }
}

fn read_u16(b: &[u8], off: usize) -> Result<u16> {
    let s = b
        .get(off..off + 2)
        .ok_or_else(|| anyhow::anyhow!("APSB truncated at offset {off}"))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32(b: &[u8], off: usize) -> Result<u32> {
    let s = b
        .get(off..off + 4)
        .ok_or_else(|| anyhow::anyhow!("APSB truncated at offset {off}"))?;
    Ok(u32::from_le_bytes(s.try_into().unwrap()))
}

fn read_u64(b: &[u8], off: usize) -> Result<u64> {
    let s = b
        .get(off..off + 8)
        .ok_or_else(|| anyhow::anyhow!("APSB truncated at offset {off}"))?;
    Ok(u64::from_le_bytes(s.try_into().unwrap()))
}

/// Decode a fixed-width, NUL-terminated UTF-8 string field.
fn read_cstr(b: &[u8], off: usize, len: usize) -> Result<String> {
    let s = b
        .get(off..off + len)
        .ok_or_else(|| anyhow::anyhow!("APSB name field truncated"))?;
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    Ok(String::from_utf8_lossy(&s[..end]).into_owned())
}
