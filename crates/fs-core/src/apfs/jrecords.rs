//! APFS file-system tree records (`j_*`).
//!
//! The file-system B-tree is keyed by `j_key_t`: a single u64 packing
//! the object id (low 60 bits) and a record type (high 4 bits). For a
//! given inode you'll find an INODE record, its directory entries
//! (DIR_REC) keyed by the *parent* directory's oid, extent records,
//! xattrs, etc. We parse the subset needed to list directories and
//! stat inodes here; file extents land in M3.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Inode number of a volume's root directory.
pub const ROOT_DIR_INO: u64 = 2;

// j_obj_types — the 4-bit record type in the high bits of j_key.
pub const APFS_TYPE_INODE: u8 = 3;
pub const APFS_TYPE_XATTR: u8 = 4;
pub const APFS_TYPE_DSTREAM_ID: u8 = 6;
pub const APFS_TYPE_FILE_EXTENT: u8 = 8;
pub const APFS_TYPE_DIR_REC: u8 = 9;

/// `j_key_t`: `obj_id` in the low 60 bits, record `kind` in the high 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JKey {
    pub obj_id: u64,
    pub kind: u8,
}

impl JKey {
    pub fn parse(key: &[u8]) -> Option<Self> {
        let raw = u64::from_le_bytes(key.get(0..8)?.try_into().unwrap());
        Some(Self {
            obj_id: raw & 0x0fff_ffff_ffff_ffff,
            kind: (raw >> 60) as u8,
        })
    }
}

// d_type values stored in j_drec_val.flags (low bits).
const DREC_TYPE_MASK: u16 = 0x000f;
const DT_DIR: u16 = 4;

/// A decoded directory entry (DIR_REC leaf record).
#[derive(Debug, Clone)]
pub struct DirRec {
    pub name: String,
    /// Child object id (inode number) this entry points at.
    pub file_id: u64,
    pub is_dir: bool,
}

/// Parse a DIR_REC key's name. `hashed` selects the on-disk key form:
/// case-insensitive / normalization-insensitive volumes use
/// `j_drec_hashed_key_t` (u32 name_len+hash); others use a bare u16
/// name_len. The name is NUL-terminated and counted *including* the NUL.
pub fn parse_drec_name(key: &[u8], hashed: bool) -> Option<String> {
    // key[0..8] is the j_key header.
    let (name_len, name_off) = if hashed {
        let field = u32::from_le_bytes(key.get(8..12)?.try_into().unwrap());
        ((field & 0x3ff) as usize, 12)
    } else {
        (
            u16::from_le_bytes(key.get(8..10)?.try_into().unwrap()) as usize,
            10,
        )
    };
    let raw = key.get(name_off..name_off + name_len)?;
    // Drop the trailing NUL.
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    Some(String::from_utf8_lossy(&raw[..end]).into_owned())
}

/// Parse a DIR_REC value: child file id + whether it's a directory.
pub fn parse_drec_val(val: &[u8]) -> Option<(u64, bool)> {
    let file_id = u64::from_le_bytes(val.get(0..8)?.try_into().unwrap());
    // val[8..16] = date_added, val[16..18] = flags
    let flags = u16::from_le_bytes(val.get(16..18)?.try_into().unwrap());
    let is_dir = flags & DREC_TYPE_MASK == DT_DIR;
    Some((file_id, is_dir))
}

// Mode bits (st_mode) we care about.
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;

/// Fixed fields of `j_inode_val_t` we use. Offsets within the value.
const INO_OFF_MOD_TIME: usize = 24;
const INO_OFF_CREATE_TIME: usize = 16;
const INO_OFF_MODE: usize = 80;

/// Decoded inode metadata (the fields M2 needs).
#[derive(Debug, Clone)]
pub struct InodeVal {
    pub is_dir: bool,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
}

/// Parse the fixed portion of a `j_inode_val_t`.
pub fn parse_inode_val(val: &[u8]) -> Option<InodeVal> {
    let mode = u16::from_le_bytes(val.get(INO_OFF_MODE..INO_OFF_MODE + 2)?.try_into().unwrap());
    let create_ns = u64::from_le_bytes(
        val.get(INO_OFF_CREATE_TIME..INO_OFF_CREATE_TIME + 8)?
            .try_into()
            .unwrap(),
    );
    let mod_ns = u64::from_le_bytes(
        val.get(INO_OFF_MOD_TIME..INO_OFF_MOD_TIME + 8)?
            .try_into()
            .unwrap(),
    );
    Some(InodeVal {
        is_dir: mode & S_IFMT == S_IFDIR,
        created: ns_to_systemtime(create_ns),
        modified: ns_to_systemtime(mod_ns),
    })
}

// --- file extents + data stream ------------------------------------

/// Mask selecting the byte length in `j_file_extent_val.len_and_flags`.
const J_FILE_EXTENT_LEN_MASK: u64 = 0x00ff_ffff_ffff_ffff;

/// A file extent: a contiguous mapping of a file's logical byte range
/// to physical blocks.
#[derive(Debug, Clone, Copy)]
pub struct FileExtent {
    /// Logical byte offset within the file.
    pub logical_addr: u64,
    /// Length in bytes.
    pub len: u64,
    /// Starting physical block number (container addressing). Zero means
    /// a sparse hole.
    pub phys_block: u64,
}

/// Parse a FILE_EXTENT key → its logical byte offset (`logical_addr`).
pub fn parse_file_extent_key(key: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(key.get(8..16)?.try_into().unwrap()))
}

/// Parse a FILE_EXTENT value → `(len_bytes, phys_block)`.
pub fn parse_file_extent_val(val: &[u8]) -> Option<(u64, u64)> {
    let len_and_flags = u64::from_le_bytes(val.get(0..8)?.try_into().unwrap());
    let phys_block = u64::from_le_bytes(val.get(8..16)?.try_into().unwrap());
    Some((len_and_flags & J_FILE_EXTENT_LEN_MASK, phys_block))
}

/// Inode extended-field type for the data stream (`j_dstream_t`), whose
/// first u64 is the file's logical size.
const INO_EXT_TYPE_DSTREAM: u8 = 8;
/// Fixed portion of `j_inode_val_t` before the extended fields.
const INODE_FIXED_LEN: usize = 92;

fn align8(x: usize) -> usize {
    (x + 7) & !7
}

/// Extract a file inode's logical size from its data-stream extended
/// field (`INO_EXT_TYPE_DSTREAM`). Returns `None` if the inode has no
/// data stream (e.g. an empty file or a directory).
pub fn parse_inode_size(val: &[u8]) -> Option<u64> {
    // xf_blob_t follows the fixed inode: { u16 num_exts; u16 used_data; }
    let num_exts = u16::from_le_bytes(
        val.get(INODE_FIXED_LEN..INODE_FIXED_LEN + 2)?
            .try_into()
            .unwrap(),
    ) as usize;
    let headers = INODE_FIXED_LEN + 4;
    // Values follow the header array, 8-byte aligned.
    let mut value_off = align8(headers + num_exts * 4);
    for i in 0..num_exts {
        let h = headers + i * 4;
        let x_type = *val.get(h)?;
        let x_size = u16::from_le_bytes(val.get(h + 2..h + 4)?.try_into().unwrap()) as usize;
        if x_type == INO_EXT_TYPE_DSTREAM {
            // j_dstream_t.size is the first u64.
            return Some(u64::from_le_bytes(
                val.get(value_off..value_off + 8)?.try_into().unwrap(),
            ));
        }
        value_off += align8(x_size);
    }
    None
}

/// APFS timestamps are nanoseconds since the Unix epoch.
fn ns_to_systemtime(ns: u64) -> Option<SystemTime> {
    if ns == 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_nanos(ns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jkey_splits_oid_and_type() {
        let raw: u64 = ((APFS_TYPE_DIR_REC as u64) << 60) | 0x123;
        let k = JKey::parse(&raw.to_le_bytes()).unwrap();
        assert_eq!(k.obj_id, 0x123);
        assert_eq!(k.kind, APFS_TYPE_DIR_REC);
    }

    #[test]
    fn drec_hashed_name_decodes() {
        // j_key(8) + name_len_and_hash(u32) + "hi\0"
        let mut key = vec![0u8; 8];
        let name = b"hi\0";
        let field: u32 = (name.len() as u32) & 0x3ff; // hash bits zero
        key.extend_from_slice(&field.to_le_bytes());
        key.extend_from_slice(name);
        assert_eq!(parse_drec_name(&key, true).unwrap(), "hi");
    }

    #[test]
    fn drec_val_reads_file_id_and_dir_flag() {
        let mut v = vec![0u8; 18];
        v[0..8].copy_from_slice(&99u64.to_le_bytes());
        v[16..18].copy_from_slice(&DT_DIR.to_le_bytes());
        let (id, is_dir) = parse_drec_val(&v).unwrap();
        assert_eq!(id, 99);
        assert!(is_dir);
    }
}
