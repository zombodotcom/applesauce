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
//! ## Reads
//!
//! File sizes come from the inode's data-stream extended field; reads
//! map a logical offset to a FILE_EXTENT (physical block run) and copy
//! the bytes, zero-filling sparse holes. Parsed fs-tree nodes and
//! resolved oid→block mappings are cached so repeated walks (listing a
//! directory stats every child) stay cheap.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use quick_cache::sync::Cache;

use crate::{DirEntry, MacFilesystem, Stat};

use super::btree::{VarKvNode, BTREE_INFO_SIZE};
use super::jrecords::{
    parse_drec_name, parse_drec_val, parse_file_extent_key, parse_file_extent_val,
    parse_inode_size, parse_inode_val, DirRec, FileExtent, InodeVal, JKey, APFS_TYPE_DIR_REC,
    APFS_TYPE_FILE_EXTENT, APFS_TYPE_INODE, ROOT_DIR_INO,
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

/// fs-tree node cache (each node is one block, ~4 KiB). The root and
/// upper index levels are touched on every query, so this turns a
/// directory listing from N tree descents into mostly cache hits.
const NODE_CACHE_CAPACITY: usize = 4096;
/// Cache of resolved virtual-oid → block-address mappings, so we don't
/// re-descend the volume object map for hot nodes.
const PADDR_CACHE_CAPACITY: usize = 16384;
/// Cache of `list_children` results by directory oid. Explorer re-lists
/// directories on sort/filter/pane changes, and path resolution lists
/// each component dir — without this every one re-walks the tree.
const CHILDREN_CACHE_CAPACITY: usize = 1024;
/// Cache of resolved POSIX path → (oid, is_dir). WinFsp does
/// get_security_by_name → open → get_file_info on the same path
/// back-to-back; this collapses those to one walk.
const PATH_CACHE_CAPACITY: usize = 4096;
/// Cache of inode oid → logical size. Listing a directory stats every
/// child for its size; caching avoids re-reading inodes on re-list.
const SIZE_CACHE_CAPACITY: usize = 65536;

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
    node_cache: Cache<Oid, Arc<VarKvNode>>,
    paddr_cache: Cache<Oid, Paddr>,
    children_cache: Cache<Oid, Arc<Vec<DirRec>>>,
    path_cache: Cache<String, (Oid, bool)>,
    size_cache: Cache<Oid, u64>,
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
            node_cache: Cache::new(NODE_CACHE_CAPACITY),
            paddr_cache: Cache::new(PADDR_CACHE_CAPACITY),
            children_cache: Cache::new(CHILDREN_CACHE_CAPACITY),
            path_cache: Cache::new(PATH_CACHE_CAPACITY),
            size_cache: Cache::new(SIZE_CACHE_CAPACITY),
        })
    }

    /// Resolve a tree node's virtual oid to its block address, via the
    /// volume object map (cached).
    fn resolve_oid(&mut self, oid: Oid) -> Result<Paddr> {
        if self.physical_tree {
            return Ok(oid as Paddr);
        }
        if let Some(p) = self.paddr_cache.get(&oid) {
            return Ok(p);
        }
        let bs = self.block_size;
        let max_xid = self.max_xid;
        let omap = &self.omap;
        let mut reader = BlockReader::new(&mut self.source, bs);
        let paddr = omap
            .lookup(&mut reader, oid, max_xid)?
            .ok_or_else(|| anyhow!("fs-tree node oid {oid} not in omap"))?
            .paddr;
        self.paddr_cache.insert(oid, paddr);
        Ok(paddr)
    }

    /// Resolve, read, and parse a tree node, caching the parsed result.
    fn read_fs_node(&mut self, oid: Oid) -> Result<Arc<VarKvNode>> {
        if let Some(node) = self.node_cache.get(&oid) {
            return Ok(node);
        }
        let paddr = self.resolve_oid(oid)?;
        let bs = self.block_size;
        let block = {
            let mut reader = BlockReader::new(&mut self.source, bs);
            reader.read_block(paddr)?
        };
        let node = Arc::new(VarKvNode::parse(block)?);
        self.node_cache.insert(oid, node.clone());
        Ok(node)
    }

    /// Read `buf.len()` bytes (or until EOF) starting at an absolute
    /// container-relative byte offset. Used for file extent data, which
    /// references physical blocks directly (not through the object map).
    fn read_at(&mut self, byte_offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.source.seek(SeekFrom::Start(byte_offset))?;
        let mut filled = 0usize;
        while filled < buf.len() {
            match self.source.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(filled)
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

    /// List a directory's entries by oid (cached).
    fn list_children(&mut self, dir_oid: Oid) -> Result<Arc<Vec<DirRec>>> {
        if let Some(cached) = self.children_cache.get(&dir_oid) {
            return Ok(cached);
        }
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
        let arc = Arc::new(out);
        self.children_cache.insert(dir_oid, arc.clone());
        Ok(arc)
    }

    /// Fetch an inode's record value bytes by oid.
    fn inode_record(&mut self, oid: Oid) -> Result<Option<Vec<u8>>> {
        let recs = self.collect_records(oid, APFS_TYPE_INODE)?;
        Ok(recs.into_iter().next().map(|(_, val)| val))
    }

    /// Logical size of a file inode (0 if it has no data stream), cached.
    fn inode_size(&mut self, oid: Oid) -> Result<u64> {
        if let Some(sz) = self.size_cache.get(&oid) {
            return Ok(sz);
        }
        let sz = self
            .inode_record(oid)?
            .and_then(|v| parse_inode_size(&v))
            .unwrap_or(0);
        self.size_cache.insert(oid, sz);
        Ok(sz)
    }

    /// All extents of a file, sorted by logical offset.
    fn file_extents(&mut self, oid: Oid) -> Result<Vec<FileExtent>> {
        let recs = self.collect_records(oid, APFS_TYPE_FILE_EXTENT)?;
        let mut exts = Vec::with_capacity(recs.len());
        for (key, val) in recs {
            let Some(logical_addr) = parse_file_extent_key(&key) else {
                continue;
            };
            let Some((len, phys_block)) = parse_file_extent_val(&val) else {
                continue;
            };
            exts.push(FileExtent {
                logical_addr,
                len,
                phys_block,
            });
        }
        exts.sort_by_key(|e| e.logical_addr);
        Ok(exts)
    }

    /// Resolve a POSIX path to `(oid, is_dir)` (cached). The root (`/`
    /// or empty) is the volume's root directory.
    fn resolve_path(&mut self, path: &str) -> Result<(Oid, bool)> {
        if let Some(hit) = self.path_cache.get(path) {
            return Ok(hit);
        }
        let case_insensitive = self.case_insensitive;
        let mut oid = ROOT_DIR_INO;
        let mut is_dir = true;
        for comp in path.split('/').filter(|s| !s.is_empty()) {
            if !is_dir {
                bail!("{path}: not a directory");
            }
            let children = self.list_children(oid)?;
            let found = children.iter().find(|c| {
                if case_insensitive {
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
        self.path_cache.insert(path.to_string(), (oid, is_dir));
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
        for c in children.iter() {
            // Files carry their size in the inode's data stream; reading
            // it per entry is what lets pull copy the right byte count.
            let size_bytes = if c.is_dir {
                0
            } else {
                self.inode_size(c.file_id)?
            };
            out.push(Stat {
                name: c.name.clone(),
                size_bytes,
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
        let record = self.inode_record(oid)?;
        let inode: Option<InodeVal> = record.as_deref().and_then(parse_inode_val);
        let size_bytes = if is_dir {
            0
        } else {
            record.as_deref().and_then(parse_inode_size).unwrap_or(0)
        };
        Ok(Stat {
            name,
            size_bytes,
            is_dir,
            modified: inode.as_ref().and_then(|i| i.modified),
            created: inode.as_ref().and_then(|i| i.created),
        })
    }

    fn read_file_range(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (oid, is_dir) = self.resolve_path(path)?;
        if is_dir {
            bail!("{path} is a directory");
        }
        let size = self.inode_size(oid)?;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let bs = self.block_size as u64;
        let extents = self.file_extents(oid)?;

        // Extent covering `offset`, if any.
        if let Some(e) = extents
            .iter()
            .find(|e| offset >= e.logical_addr && offset < e.logical_addr + e.len)
        {
            let into = offset - e.logical_addr;
            // Clamp to this extent, the file size, and the caller's buffer.
            let avail = (e.len - into).min(size - offset).min(buf.len() as u64) as usize;
            if e.phys_block == 0 {
                // Sparse hole inside the extent → zeros.
                buf[..avail].fill(0);
                return Ok(avail);
            }
            let phys_byte = e.phys_block * bs + into;
            return self.read_at(phys_byte, &mut buf[..avail]);
        }

        // Not covered by any extent: a sparse hole. Zero-fill up to the
        // next extent (or end of file), bounded by the buffer.
        let hole_end = extents
            .iter()
            .map(|e| e.logical_addr)
            .filter(|&a| a > offset)
            .min()
            .unwrap_or(size);
        let avail = (hole_end - offset).min(buf.len() as u64) as usize;
        buf[..avail].fill(0);
        Ok(avail)
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

    /// INODE record value for `oid` with a DSTREAM xfield of `size`.
    fn inode_with_size(size: u64) -> Vec<u8> {
        // 92-byte fixed inode, then xf_blob: num_exts=1, one DSTREAM
        // (type 8) xfield whose value (j_dstream) starts at offset 104.
        let mut v = vec![0u8; 104 + 40];
        // mode @80 = S_IFREG so it reads as a file.
        put_u16(&mut v, 80, 0o100000);
        put_u16(&mut v, 92, 1); // xf_num_exts
        put_u16(&mut v, 94, 40); // xf_used_data (approx)
        v[96] = 8; // x_type = INO_EXT_TYPE_DSTREAM
        v[97] = 0; // x_flags
        put_u16(&mut v, 98, 40); // x_size = sizeof(j_dstream)
        put_u64(&mut v, 104, size); // j_dstream.size
        v
    }

    fn jkey(oid: u64, kind: u8) -> [u8; 8] {
        (((kind as u64) << 60) | oid).to_le_bytes()
    }

    #[test]
    fn reads_file_contents_via_extent() {
        let mut disk = vec![0u8; BS * 8];
        place(&mut disk, 1, &omap_phys());
        place(&mut disk, 2, &omap_btree());

        let data = b"hello world";
        // File extent: logical 0, one block long, physical block 5.
        let mut ext_key = jkey(17, APFS_TYPE_FILE_EXTENT).to_vec();
        ext_key.extend_from_slice(&0u64.to_le_bytes()); // logical_addr
        let mut ext_val = Vec::new();
        ext_val.extend_from_slice(&(BS as u64).to_le_bytes()); // len_and_flags
        ext_val.extend_from_slice(&5u64.to_le_bytes()); // phys_block = 5
        ext_val.extend_from_slice(&0u64.to_le_bytes()); // crypto_id

        let recs = vec![
            (drec_key(ROOT_DIR_INO, "hello.txt"), drec_val(17, false)),
            (
                jkey(17, APFS_TYPE_INODE).to_vec(),
                inode_with_size(data.len() as u64),
            ),
            (ext_key, ext_val),
        ];
        place(&mut disk, 3, &fs_leaf(&recs));
        // File data lives in block 5.
        disk[5 * BS..5 * BS + data.len()].copy_from_slice(data);

        let mut vol = ApfsVolume::open(
            Cursor::new(disk),
            BS as u32,
            1,
            1000,
            Xid::MAX,
            true,
            true,
            "Test".to_string(),
        )
        .unwrap();

        // Size is reported from the inode's data stream.
        let st = vol.stat("/hello.txt").unwrap();
        assert_eq!(st.size_bytes, data.len() as u64);
        assert!(!st.is_dir);

        // Reading the file yields the bytes from the extent's block.
        let mut buf = vec![0u8; 64];
        let n = vol.read_file_range("/hello.txt", 0, &mut buf).unwrap();
        assert_eq!(n, data.len());
        assert_eq!(&buf[..n], data);

        // Reading at/after EOF returns 0.
        assert_eq!(vol.read_file_range("/hello.txt", 100, &mut buf).unwrap(), 0);
    }
}
