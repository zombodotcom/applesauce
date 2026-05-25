//! APFS volume driver: walks the file-system B-tree to list directories
//! and stat inodes, behind the [`MacFilesystem`] trait.
//!
//! ## Trees and resolution
//!
//! The volume's file-system tree (`apfs_root_tree_oid`) is a *virtual*
//! object tree: every node — root and children — is named by a virtual
//! oid that must be resolved to a block through the volume's object map
//! (unless the tree's `BTREE_PHYSICAL` flag is set, in which case oids
//! are block addresses directly).
//!
//! ## Listing
//!
//! Directory entries (DIR_REC) for a directory are keyed by the
//! directory's own oid. To list a directory we range-collect every
//! DIR_REC whose `j_key.obj_id` equals the directory oid, pruning
//! B-tree subtrees that can't contain that key prefix. Path resolution
//! walks components from the root inode, matching names linearly (so we
//! don't need to reimplement APFS's name-hash).
//!
//! File reads (extents) land in M3; [`read_file_range`] errors until then.

use std::io::{Read, Seek};

use anyhow::{anyhow, bail, Result};

use crate::{DirEntry, MacFilesystem, Stat};

use super::btree::{VarKvNode, BTREE_INFO_SIZE};
use super::jrecords::{
    parse_drec_name, parse_drec_val, parse_inode_val, DirRec, InodeVal, JKey, APFS_TYPE_DIR_REC,
    APFS_TYPE_INODE, ROOT_DIR_INO,
};
use super::object::BlockReader;
use super::omap::Omap;
use super::types::{Oid, Paddr, Xid};

/// `BTREE_PHYSICAL` bit in `btree_info.bt_fixed.bt_flags`: child
/// pointers are block addresses rather than virtual oids. (Bit 0x2 is
/// `BTREE_SEQUENTIAL_INSERT`, which the fs-tree sets — don't confuse
/// them; PHYSICAL is 0x10.)
const BTREE_PHYSICAL: u32 = 0x0000_0010;

/// Guard against runaway / cyclic trees during a range collect.
const MAX_NODES_PER_QUERY: usize = 100_000;

/// A read-only APFS volume.
pub struct ApfsVolume<S> {
    source: S,
    block_size: u32,
    omap: Omap,
    /// Virtual oid of the file-system tree root.
    root_tree_oid: Oid,
    /// Ceiling xid for volume-omap lookups: the volume superblock's own
    /// transaction id. Resolving objects at a higher xid would read a
    /// newer (possibly uncommitted) transaction's blocks.
    max_xid: Xid,
    /// Child pointers are physical (block addrs) rather than virtual.
    physical_tree: bool,
    hashed_drec_keys: bool,
    case_insensitive: bool,
    volume_label: String,
}

impl<S: Read + Seek + Send> ApfsVolume<S> {
    /// Open a volume given its container-relative `source` (block 0 is
    /// the container superblock), the volume's object-map block address
    /// (`apfs_omap_oid`, a physical oid), and the virtual oid of its
    /// file-system tree (`apfs_root_tree_oid`).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        mut source: S,
        block_size: u32,
        omap_paddr: Paddr,
        root_tree_oid: Oid,
        max_xid: Xid,
        hashed_drec_keys: bool,
        case_insensitive: bool,
        volume_label: String,
    ) -> Result<Self> {
        let (omap, physical_tree) = {
            let mut reader = BlockReader::new(&mut source, block_size);
            let omap = Omap::open(&mut reader, omap_paddr)?;
            // Resolve the fs-tree root to read its info trailer's flags.
            let root_paddr = omap
                .lookup(&mut reader, root_tree_oid, max_xid)?
                .ok_or_else(|| anyhow!("fs-tree root oid {root_tree_oid} not in volume omap"))?
                .paddr;
            let root_block = reader.read_block(root_paddr)?;
            let physical_tree = read_btree_physical_flag(&root_block);
            (omap, physical_tree)
        };

        Ok(Self {
            source,
            block_size,
            omap,
            root_tree_oid,
            max_xid,
            physical_tree,
            hashed_drec_keys,
            case_insensitive,
            volume_label,
        })
    }

    /// Resolve a tree node's oid to its block address, then read+parse it.
    fn read_fs_node(&mut self, oid: Oid) -> Result<VarKvNode> {
        let bs = self.block_size;
        let physical = self.physical_tree;
        let omap = &self.omap;
        let mut reader = BlockReader::new(&mut self.source, bs);
        let max_xid = self.max_xid;
        let paddr = if physical {
            oid as Paddr
        } else {
            omap.lookup(&mut reader, oid, max_xid)?
                .ok_or_else(|| anyhow!("fs-tree node oid {oid} not in omap"))?
                .paddr
        };
        let block = reader.read_block(paddr)?;
        VarKvNode::parse(block)
    }

    /// Collect every leaf record whose key prefix is `(obj_id, kind)`.
    /// Returns `(key_bytes, value_bytes)` pairs in tree order.
    fn collect_records(&mut self, obj_id: Oid, kind: u8) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut visited = 0usize;
        let mut stack = vec![self.root_tree_oid];

        while let Some(oid) = stack.pop() {
            visited += 1;
            if visited > MAX_NODES_PER_QUERY {
                bail!("fs-tree walk exceeded node budget (corrupt tree?)");
            }
            let node = self.read_fs_node(oid)?;

            if node.is_leaf() {
                for i in 0..node.nkeys() {
                    let Some(key) = node.key(i) else { continue };
                    let Some(jk) = JKey::parse(key) else { continue };
                    if jk.obj_id == obj_id && jk.kind == kind {
                        if let Some(val) = node.value(i) {
                            out.push((key.to_vec(), val.to_vec()));
                        }
                    }
                }
                continue;
            }

            // Index node: descend children whose key range can hold the
            // target prefix. Child i covers [key_i, key_{i+1}).
            let n = node.nkeys();
            for i in 0..n {
                let Some(lo) = node.key(i).and_then(JKey::parse) else {
                    continue;
                };
                let after_lo = (lo.obj_id, lo.kind) <= (obj_id, kind);
                let before_hi = match node.key(i + 1).and_then(JKey::parse) {
                    Some(hi) => (obj_id, kind) <= (hi.obj_id, hi.kind),
                    None => true, // last child: open upper bound
                };
                if after_lo && before_hi {
                    if let Some(child) = node.child_oid(i) {
                        stack.push(child);
                    }
                }
            }
        }
        Ok(out)
    }

    /// List a directory's entries by oid.
    fn list_children(&mut self, dir_oid: Oid) -> Result<Vec<DirRec>> {
        let hashed = self.hashed_drec_keys;
        let recs = self.collect_records(dir_oid, APFS_TYPE_DIR_REC)?;
        let mut out = Vec::with_capacity(recs.len());
        for (key, val) in recs {
            let Some(name) = parse_drec_name(&key, hashed) else {
                continue;
            };
            let Some((file_id, is_dir)) = parse_drec_val(&val) else {
                continue;
            };
            out.push(DirRec {
                name,
                file_id,
                is_dir,
            });
        }
        Ok(out)
    }

    /// Read an inode record by oid.
    fn read_inode(&mut self, oid: Oid) -> Result<Option<InodeVal>> {
        let recs = self.collect_records(oid, APFS_TYPE_INODE)?;
        Ok(recs.first().and_then(|(_, val)| parse_inode_val(val)))
    }

    /// Resolve a POSIX path to `(oid, is_dir)`. The root (`/` or empty)
    /// is the volume's root directory.
    fn resolve_path(&mut self, path: &str) -> Result<(Oid, bool)> {
        let mut oid = ROOT_DIR_INO;
        let mut is_dir = true;
        for comp in path.split('/').filter(|s| !s.is_empty()) {
            if !is_dir {
                bail!("{path}: not a directory");
            }
            let children = self.list_children(oid)?;
            let found = children.into_iter().find(|c| {
                if self.case_insensitive {
                    c.name.eq_ignore_ascii_case(comp)
                } else {
                    c.name == comp
                }
            });
            match found {
                Some(c) => {
                    oid = c.file_id;
                    is_dir = c.is_dir;
                }
                None => bail!("path not found: {path} (no entry {comp:?})"),
            }
        }
        Ok((oid, is_dir))
    }
}

impl<S: Read + Seek + Send> MacFilesystem for ApfsVolume<S> {
    fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let (oid, is_dir) = self.resolve_path(path)?;
        if !is_dir {
            bail!("{path} is not a directory");
        }
        let children = self.list_children(oid)?;
        let mut out = Vec::with_capacity(children.len());
        for c in children {
            out.push(Stat {
                name: c.name,
                size_bytes: 0, // file sizes need extent/dstream reads (M3)
                is_dir: c.is_dir,
                modified: None,
                created: None,
            });
        }
        Ok(out)
    }

    fn stat(&mut self, path: &str) -> Result<Stat> {
        let (oid, is_dir) = self.resolve_path(path)?;
        let name = path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let inode = self.read_inode(oid)?;
        Ok(Stat {
            name,
            size_bytes: 0, // M3
            is_dir,
            modified: inode.as_ref().and_then(|i| i.modified),
            created: inode.as_ref().and_then(|i| i.created),
        })
    }

    fn read_file_range(&mut self, _path: &str, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
        bail!("APFS file reads are not implemented yet (M3)");
    }

    fn volume_label(&self) -> Option<&str> {
        Some(&self.volume_label)
    }
}

/// Read the `BTREE_PHYSICAL` flag from a root node's `btree_info`
/// trailer (the last [`BTREE_INFO_SIZE`] bytes; `bt_flags` is its first
/// u32). Non-root nodes don't carry the trailer; treat as virtual.
fn read_btree_physical_flag(root_block: &[u8]) -> bool {
    let len = root_block.len();
    if len < BTREE_INFO_SIZE {
        return false;
    }
    let off = len - BTREE_INFO_SIZE;
    let flags = u32::from_le_bytes(root_block[off..off + 4].try_into().unwrap());
    flags & BTREE_PHYSICAL != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apfs::btree::{BTNODE_FIXED_KV_SIZE, BTNODE_LEAF, BTNODE_ROOT};
    use std::io::Cursor;

    const BS: usize = 4096;
    const INFO: usize = BTREE_INFO_SIZE;

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
    fn place(disk: &mut [u8], idx: usize, block: &[u8]) {
        disk[idx * BS..idx * BS + BS].copy_from_slice(block);
    }

    /// Volume omap_phys whose b-tree root is at block 2.
    fn omap_phys() -> Vec<u8> {
        let mut b = vec![0u8; BS];
        put_u64(&mut b, 48, 2); // om_tree_oid = block 2
        b
    }

    /// Fixed-KV omap leaf mapping fs-tree root oid 1000 → block 3.
    fn omap_btree() -> Vec<u8> {
        let mut b = vec![0u8; BS];
        put_u16(&mut b, 32, BTNODE_ROOT | BTNODE_LEAF | BTNODE_FIXED_KV_SIZE);
        put_u32(&mut b, 36, 1); // nkeys
        put_u16(&mut b, 40, 0); // table_space.off
        put_u16(&mut b, 42, 8); // table_space.len
        put_u16(&mut b, 56, 0); // toc[0].k
        put_u16(&mut b, 58, 16); // toc[0].v
        put_u64(&mut b, 64, 1000); // omap_key.oid
        put_u64(&mut b, 72, 1); // omap_key.xid
        let val = BS - INFO - 16;
        put_u32(&mut b, val, 0); // ov_flags
        put_u32(&mut b, val + 4, BS as u32); // ov_size
        put_i64(&mut b, val + 8, 3); // ov_paddr = block 3
        b
    }

    /// Build a hashed DIR_REC key for parent dir `oid` and `name`.
    fn drec_key(oid: u64, name: &str) -> Vec<u8> {
        let mut k = Vec::new();
        let raw = ((APFS_TYPE_DIR_REC as u64) << 60) | oid;
        k.extend_from_slice(&raw.to_le_bytes());
        let mut nm = name.as_bytes().to_vec();
        nm.push(0); // NUL terminator, counted in name_len
        let field = (nm.len() as u32) & 0x3ff;
        k.extend_from_slice(&field.to_le_bytes());
        k.extend_from_slice(&nm);
        k
    }

    /// Build a DIR_REC value: file_id + dir/file flag.
    fn drec_val(file_id: u64, is_dir: bool) -> Vec<u8> {
        let mut v = vec![0u8; 18];
        v[0..8].copy_from_slice(&file_id.to_le_bytes());
        let flags: u16 = if is_dir { 4 } else { 8 };
        v[16..18].copy_from_slice(&flags.to_le_bytes());
        v
    }

    /// Variable-KV fs-tree root+leaf holding the given (key,val) records.
    fn fs_leaf(records: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![0u8; BS];
        put_u16(&mut b, 32, BTNODE_ROOT | BTNODE_LEAF); // variable-KV (no FIXED flag)
        put_u32(&mut b, 36, records.len() as u32);
        let toc_len = (records.len() * 8) as u16;
        put_u16(&mut b, 40, 0); // table_space.off
        put_u16(&mut b, 42, toc_len);

        let key_area = 56 + toc_len as usize;
        let val_end = BS - INFO;
        let mut k_cursor = 0usize; // offset within key area
        let mut v_cursor = 0usize; // offset from val_end, growing
        for (i, (key, val)) in records.iter().enumerate() {
            // TOC entry i (kvloc: k_off,k_len,v_off,v_len)
            let toc = 56 + i * 8;
            let v_off = v_cursor + val.len();
            put_u16(&mut b, toc, k_cursor as u16);
            put_u16(&mut b, toc + 2, key.len() as u16);
            put_u16(&mut b, toc + 4, v_off as u16);
            put_u16(&mut b, toc + 6, val.len() as u16);
            // key bytes
            b[key_area + k_cursor..key_area + k_cursor + key.len()].copy_from_slice(key);
            // value bytes at val_end - v_off
            let vstart = val_end - v_off;
            b[vstart..vstart + val.len()].copy_from_slice(val);
            k_cursor += key.len();
            v_cursor += val.len();
        }
        // btree_info trailer left zeroed → BTREE_PHYSICAL clear (virtual).
        b
    }

    fn build_volume() -> ApfsVolume<Cursor<Vec<u8>>> {
        let mut disk = vec![0u8; BS * 8];
        place(&mut disk, 1, &omap_phys());
        place(&mut disk, 2, &omap_btree());
        let recs = vec![
            (drec_key(ROOT_DIR_INO, "Documents"), drec_val(16, true)),
            (drec_key(ROOT_DIR_INO, "file.txt"), drec_val(17, false)),
        ];
        place(&mut disk, 3, &fs_leaf(&recs));

        ApfsVolume::open(
            Cursor::new(disk),
            BS as u32,
            1,        // omap_phys at block 1
            1000,     // root_tree_oid
            Xid::MAX, // ceiling (fixture omap entry has xid 1)
            true,     // hashed_drec_keys
            true,     // case_insensitive
            "Test".to_string(),
        )
        .unwrap()
    }

    #[test]
    fn lists_root_directory() {
        let mut vol = build_volume();
        let mut entries = vol.list_dir("/").unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Documents");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "file.txt");
        assert!(!entries[1].is_dir);
    }

    #[test]
    fn resolves_path_case_insensitively() {
        let mut vol = build_volume();
        // case-insensitive lookup should find "Documents" via "documents"
        let (oid, is_dir) = vol.resolve_path("/documents").unwrap();
        assert_eq!(oid, 16);
        assert!(is_dir);
    }

    #[test]
    fn missing_path_errors() {
        let mut vol = build_volume();
        assert!(vol.list_dir("/nope").is_err());
    }
}
