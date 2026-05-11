//! HFS+ catalog record parsing.
//!
//! Records in the catalog B-tree have the form:
//!
//! ```text
//!   HFSPlusCatalogKey  ||  record_data
//!         |                     |
//!         |                     +-- leaf: u16 recordType + body
//!         |                     +-- index: u32 child_node_number
//!         +-- variable size (depends on filename length)
//! ```
//!
//! Multi-byte integers are big-endian; filenames are UTF-16BE.
//!
//! ## Case folding
//!
//! HFS+ key comparison uses Apple's bespoke case-folding table from
//! Technote 1150 Appendix B. For v0 we use an ASCII-only fold (good
//! for English filenames, byte-identical-otherwise). Non-ASCII names
//! still parse and display correctly — they just won't fold against
//! their case-variants during lookup. Full TN1150 folding is a
//! follow-on task.

use std::cmp::Ordering;
use std::io::Cursor;

use anyhow::{anyhow, bail, Result};
use binrw::{BinRead, BinReaderExt};

use super::fork::HFSPlusForkData;
use super::types::HfsCatalogNodeID;

const RECORD_TYPE_FOLDER: u16 = 1;
const RECORD_TYPE_FILE: u16 = 2;
const RECORD_TYPE_FOLDER_THREAD: u16 = 3;
const RECORD_TYPE_FILE_THREAD: u16 = 4;

/// A catalog B-tree key: identifies a record by (parent CNID, filename).
#[derive(Debug, Clone)]
pub struct CatalogKey {
    pub parent_id: HfsCatalogNodeID,
    /// Filename as raw UTF-16 code units (big-endian decoded to native).
    pub name_utf16: Vec<u16>,
}

impl CatalogKey {
    /// Filename decoded to UTF-8 (lossy for invalid surrogates).
    pub fn name(&self) -> String {
        String::from_utf16_lossy(&self.name_utf16)
    }

    /// Synthesize a comparison key with an empty name. Useful for
    /// finding the first record under a parent.
    pub fn parent_only(parent_id: HfsCatalogNodeID) -> Self {
        Self {
            parent_id,
            name_utf16: Vec::new(),
        }
    }

    /// Build a key from a parent CNID and a UTF-8 name.
    pub fn from_utf8(parent_id: HfsCatalogNodeID, name: &str) -> Self {
        Self {
            parent_id,
            name_utf16: name.encode_utf16().collect(),
        }
    }
}

/// Parse a catalog key from the start of `bytes`. Returns the parsed
/// key and the number of bytes consumed (so the caller can find the
/// record data that follows).
pub fn parse_key(bytes: &[u8]) -> Result<(CatalogKey, usize)> {
    if bytes.len() < 8 {
        bail!("catalog key buffer too small: {} bytes", bytes.len());
    }
    let key_length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let total = 2 + key_length;
    if total > bytes.len() {
        bail!(
            "catalog key claims {} bytes, only {} available",
            total,
            bytes.len()
        );
    }
    let parent_id = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    let name_len = u16::from_be_bytes([bytes[6], bytes[7]]) as usize;
    let name_bytes_needed = 8 + name_len * 2;
    if name_bytes_needed > total {
        bail!(
            "catalog key claims name of {} units but only {} bytes left",
            name_len,
            total - 8
        );
    }
    let mut name_utf16 = Vec::with_capacity(name_len);
    for i in 0..name_len {
        let lo = 8 + i * 2;
        name_utf16.push(u16::from_be_bytes([bytes[lo], bytes[lo + 1]]));
    }
    Ok((
        CatalogKey {
            parent_id,
            name_utf16,
        },
        total,
    ))
}

/// HFS+ case-insensitive comparison. Only ASCII letters are folded;
/// other code units compare numerically. See module docstring.
pub fn compare_keys(a: &CatalogKey, b: &CatalogKey, case_sensitive: bool) -> Ordering {
    match a.parent_id.cmp(&b.parent_id) {
        Ordering::Equal => {}
        other => return other,
    }
    compare_names(&a.name_utf16, &b.name_utf16, case_sensitive)
}

fn compare_names(a: &[u16], b: &[u16], case_sensitive: bool) -> Ordering {
    let n = a.len().min(b.len());
    for i in 0..n {
        let mut ai = a[i];
        let mut bi = b[i];
        if !case_sensitive {
            if (b'A' as u16..=b'Z' as u16).contains(&ai) {
                ai += b'a' as u16 - b'A' as u16;
            }
            if (b'A' as u16..=b'Z' as u16).contains(&bi) {
                bi += b'a' as u16 - b'A' as u16;
            }
        }
        match ai.cmp(&bi) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// Parsed contents of a catalog record (after the key).
#[derive(Debug, Clone)]
pub enum CatalogRecord {
    Folder(FolderRecord),
    File(FileRecord),
    /// Thread records map a CNID back to its parent and name. Used to
    /// resolve "what's this CNID's path".
    FolderThread(ThreadRecord),
    FileThread(ThreadRecord),
}

#[derive(BinRead, Debug, Clone)]
#[brw(big)]
pub struct FolderRecord {
    pub flags: u16,
    pub valence: u32,
    pub folder_id: HfsCatalogNodeID,
    pub create_date: u32,
    pub content_mod_date: u32,
    pub attribute_mod_date: u32,
    pub access_date: u32,
    pub backup_date: u32,
    pub permissions: HFSPlusBSDInfo,
    pub user_info: [u8; 16],
    pub finder_info: [u8; 16],
    pub text_encoding: u32,
    pub _reserved: u32,
}

#[derive(BinRead, Debug, Clone)]
#[brw(big)]
pub struct FileRecord {
    pub flags: u16,
    pub _reserved1: u32,
    pub file_id: HfsCatalogNodeID,
    pub create_date: u32,
    pub content_mod_date: u32,
    pub attribute_mod_date: u32,
    pub access_date: u32,
    pub backup_date: u32,
    pub permissions: HFSPlusBSDInfo,
    pub user_info: [u8; 16],
    pub finder_info: [u8; 16],
    pub text_encoding: u32,
    pub _reserved2: u32,
    pub data_fork: HFSPlusForkData,
    pub resource_fork: HFSPlusForkData,
}

#[derive(BinRead, Debug, Clone, Copy)]
#[brw(big)]
pub struct HFSPlusBSDInfo {
    pub owner_id: u32,
    pub group_id: u32,
    pub admin_flags: u8,
    pub owner_flags: u8,
    pub file_mode: u16,
    pub special: u32,
}

/// Thread record body — points from a CNID back to its parent + name.
#[derive(Debug, Clone)]
pub struct ThreadRecord {
    pub parent_id: HfsCatalogNodeID,
    pub name_utf16: Vec<u16>,
}

/// Parse the record-data portion of a catalog leaf record (the bytes
/// after the key). Returns `None` if the data is too short or the
/// record type is unknown — corrupt records shouldn't crash a recovery
/// tool, just be skipped.
pub fn parse_record_data(data: &[u8]) -> Result<CatalogRecord> {
    if data.len() < 2 {
        bail!("catalog record data too small: {} bytes", data.len());
    }
    let record_type = u16::from_be_bytes([data[0], data[1]]);
    let body = &data[2..];
    match record_type {
        RECORD_TYPE_FOLDER => {
            let mut c = Cursor::new(body);
            let r: FolderRecord = c
                .read_be()
                .map_err(|e| anyhow!("decoding folder record: {e}"))?;
            Ok(CatalogRecord::Folder(r))
        }
        RECORD_TYPE_FILE => {
            let mut c = Cursor::new(body);
            let r: FileRecord = c
                .read_be()
                .map_err(|e| anyhow!("decoding file record: {e}"))?;
            Ok(CatalogRecord::File(r))
        }
        RECORD_TYPE_FOLDER_THREAD | RECORD_TYPE_FILE_THREAD => {
            // 2 bytes reserved, 4 bytes parent_id, then HFSUniStr255.
            if body.len() < 8 {
                bail!("thread record too small: {} bytes", body.len());
            }
            let parent_id = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
            let name_len = u16::from_be_bytes([body[6], body[7]]) as usize;
            let need = 8 + name_len * 2;
            if need > body.len() {
                bail!("thread record name overflows body");
            }
            let mut name_utf16 = Vec::with_capacity(name_len);
            for i in 0..name_len {
                let lo = 8 + i * 2;
                name_utf16.push(u16::from_be_bytes([body[lo], body[lo + 1]]));
            }
            let t = ThreadRecord {
                parent_id,
                name_utf16,
            };
            if record_type == RECORD_TYPE_FOLDER_THREAD {
                Ok(CatalogRecord::FolderThread(t))
            } else {
                Ok(CatalogRecord::FileThread(t))
            }
        }
        other => bail!("unknown catalog record type 0x{:04X}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_bytes(parent_id: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let key_len: u16 = 4 + 2 + units.len() as u16 * 2;
        let mut out = Vec::new();
        out.extend_from_slice(&key_len.to_be_bytes());
        out.extend_from_slice(&parent_id.to_be_bytes());
        out.extend_from_slice(&(units.len() as u16).to_be_bytes());
        for u in units {
            out.extend_from_slice(&u.to_be_bytes());
        }
        out
    }

    #[test]
    fn parses_simple_key() {
        let bytes = key_bytes(2, "Hello");
        let (key, used) = parse_key(&bytes).unwrap();
        assert_eq!(key.parent_id, 2);
        assert_eq!(key.name(), "Hello");
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn key_comparison_orders_by_parent_then_name() {
        let a = CatalogKey::from_utf8(2, "apple");
        let b = CatalogKey::from_utf8(2, "banana");
        let c = CatalogKey::from_utf8(3, "apple");
        assert_eq!(compare_keys(&a, &b, false), Ordering::Less);
        assert_eq!(compare_keys(&a, &c, false), Ordering::Less);
        assert_eq!(compare_keys(&b, &c, false), Ordering::Less);
    }

    #[test]
    fn case_fold_only_for_case_insensitive() {
        let lower = CatalogKey::from_utf8(2, "readme.txt");
        let upper = CatalogKey::from_utf8(2, "README.TXT");
        assert_eq!(compare_keys(&lower, &upper, false), Ordering::Equal);
        assert_ne!(compare_keys(&lower, &upper, true), Ordering::Equal);
    }

    #[test]
    fn parses_thread_record() {
        // record_type=3, reserved=0, parent_id=2, name="root"
        let units: Vec<u16> = "root".encode_utf16().collect();
        let mut body = Vec::new();
        body.extend_from_slice(&3u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&(units.len() as u16).to_be_bytes());
        for u in &units {
            body.extend_from_slice(&u.to_be_bytes());
        }
        let r = parse_record_data(&body).unwrap();
        match r {
            CatalogRecord::FolderThread(t) => {
                assert_eq!(t.parent_id, 2);
                assert_eq!(String::from_utf16_lossy(&t.name_utf16), "root");
            }
            _ => panic!("expected FolderThread"),
        }
    }

    #[test]
    fn rejects_unknown_record_type() {
        let body: Vec<u8> = vec![0x00, 0x99, 0x00, 0x00];
        let err = parse_record_data(&body).unwrap_err();
        assert!(err.to_string().contains("unknown catalog record type"));
    }
}
