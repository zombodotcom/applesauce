//! HFS+ filesystem driver: ties volume header + catalog B-tree + fork
//! reader together into a `MacFilesystem` implementation.

use std::cmp::Ordering;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{anyhow, bail, Result};

use crate::{DirEntry, MacFilesystem, Stat};

use super::btree::{read_btree_header, read_node, BTreeHeaderRecord, NodeKind};
use super::catalog::{compare_keys, parse_key, parse_record_data, CatalogKey, CatalogRecord};
use super::extents::{ExtentsBTree, FORK_TYPE_DATA};
use super::fork_reader::ForkReader;
use super::types::{HfsCatalogNodeID, ROOT_FOLDER_ID, VOLUME_HEADER_OFFSET};
use super::volume::{self, HFSPlusVolumeHeader};

/// Read-only HFS+ filesystem driver.
pub struct Hfsplus<S> {
    source: S,
    volume_offset: u64,
    volume_header: HFSPlusVolumeHeader,
    catalog_header: BTreeHeaderRecord,
    extents_btree: ExtentsBTree,
    volume_label: Option<String>,
}

impl<S: Read + Seek + Send> Hfsplus<S> {
    /// Open an HFS+ volume that begins at `volume_offset` bytes into
    /// `source`. Reads and validates the volume header and catalog
    /// B-tree header eagerly; lazy work happens during list/read calls.
    pub fn open(mut source: S, volume_offset: u64) -> Result<Self> {
        // Mac OS 8.1–10.3 wrote HFS+ volumes wrapped inside an HFS
        // classic shell. The outer signature is 'BD'; the inner HFS+
        // volume's offset lives in the MDB's drEmbedExtent. If that
        // wrapper is present, shift our effective volume_offset to the
        // embedded volume so everything downstream sees a normal HFS+
        // layout.
        let effective_offset = match volume::detect_hfs_wrapper(&mut source, volume_offset)? {
            Some(embedded) => embedded,
            None => volume_offset,
        };

        let volume_header =
            HFSPlusVolumeHeader::read_at(&mut source, effective_offset + VOLUME_HEADER_OFFSET)?;

        let catalog_header = {
            let mut catalog_reader = ForkReader::from_fork(
                &mut source,
                effective_offset,
                volume_header.block_size,
                volume_header.catalog_file,
            );
            let (_desc, hdr) = read_btree_header(&mut catalog_reader)?;
            hdr
        };

        let extents_btree = ExtentsBTree::open(
            &mut source,
            effective_offset,
            volume_header.block_size,
            volume_header.extents_file,
        )?;

        let mut fs = Self {
            source,
            volume_offset: effective_offset,
            volume_header,
            catalog_header,
            extents_btree,
            volume_label: None,
        };
        fs.volume_label = fs.read_volume_label().ok();
        Ok(fs)
    }

    fn read_volume_label(&mut self) -> Result<String> {
        // The thread record for the root folder (CNID 2) carries the
        // volume name in its `name_utf16` field.
        let key = CatalogKey::parent_only(ROOT_FOLDER_ID);
        let rec = self
            .lookup_exact(&key)?
            .ok_or_else(|| anyhow!("no thread record for root folder"))?;
        match rec {
            CatalogRecord::FolderThread(t) => Ok(String::from_utf16_lossy(&t.name_utf16)),
            _ => bail!("root-folder thread record was the wrong kind"),
        }
    }

    fn case_sensitive(&self) -> bool {
        self.volume_header.is_case_sensitive()
    }

    /// Descend the catalog B-tree to the first leaf position where a
    /// record with key >= `target` lives. Returns (leaf_node, idx).
    fn descend_to_leaf(&mut self, target: &CatalogKey) -> Result<(u32, usize)> {
        let case_sensitive = self.case_sensitive();
        let mut node_num = self.catalog_header.root_node;
        let node_size = self.catalog_header.node_size;

        // Avoid unbounded recursion / cycles.
        for _ in 0..64 {
            let node = self.read_catalog_node(node_num)?;
            match node.descriptor.kind_enum() {
                Some(NodeKind::Index) => {
                    let mut chosen: Option<u32> = None;
                    for i in 0..node.num_records() {
                        let record = node.record(i).ok_or_else(|| {
                            anyhow!("missing index record {i} in node {node_num}")
                        })?;
                        let (key, key_size) = parse_key(record)?;
                        match compare_keys(&key, target, case_sensitive) {
                            Ordering::Less | Ordering::Equal => {
                                let bytes = record
                                    .get(key_size..key_size + 4)
                                    .ok_or_else(|| anyhow!("short index record"))?;
                                chosen = Some(u32::from_be_bytes([
                                    bytes[0], bytes[1], bytes[2], bytes[3],
                                ]));
                            }
                            Ordering::Greater => break,
                        }
                    }
                    node_num = match chosen {
                        Some(c) => c,
                        // Target is smaller than every key; pick the
                        // leftmost child.
                        None => {
                            let first =
                                node.record(0).ok_or_else(|| anyhow!("empty index node"))?;
                            let (_, key_size) = parse_key(first)?;
                            let bytes = first
                                .get(key_size..key_size + 4)
                                .ok_or_else(|| anyhow!("short index record"))?;
                            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                        }
                    };
                    let _ = node_size;
                }
                Some(NodeKind::Leaf) => {
                    for i in 0..node.num_records() {
                        let record = node
                            .record(i)
                            .ok_or_else(|| anyhow!("missing leaf record"))?;
                        let (key, _) = parse_key(record)?;
                        if !matches!(compare_keys(&key, target, case_sensitive), Ordering::Less) {
                            return Ok((node_num, i));
                        }
                    }
                    // Spill into the next leaf.
                    let next = node.descriptor.f_link;
                    if next == 0 {
                        return Ok((node_num, node.num_records()));
                    }
                    return Ok((next, 0));
                }
                other => bail!("unexpected node kind {:?} during descent", other),
            }
        }
        bail!("catalog descent exceeded depth limit (cycle?)");
    }

    /// Look up a record by exact key. Returns `None` if no record
    /// matches.
    fn lookup_exact(&mut self, target: &CatalogKey) -> Result<Option<CatalogRecord>> {
        let case_sensitive = self.case_sensitive();
        let (leaf_num, idx) = self.descend_to_leaf(target)?;
        let node = self.read_catalog_node(leaf_num)?;
        if idx >= node.num_records() {
            return Ok(None);
        }
        let record = node
            .record(idx)
            .ok_or_else(|| anyhow!("missing leaf record"))?;
        let (key, key_size) = parse_key(record)?;
        if compare_keys(&key, target, case_sensitive) != Ordering::Equal {
            return Ok(None);
        }
        let data = &record[key_size..];
        Ok(Some(parse_record_data(data)?))
    }

    /// Collect all child folder/file records of `parent_cnid`.
    fn list_children(
        &mut self,
        parent_cnid: HfsCatalogNodeID,
    ) -> Result<Vec<(CatalogKey, CatalogRecord)>> {
        let target = CatalogKey::parent_only(parent_cnid);
        let (mut leaf_num, mut start_idx) = self.descend_to_leaf(&target)?;
        let mut out = Vec::new();

        loop {
            let node = self.read_catalog_node(leaf_num)?;
            for i in start_idx..node.num_records() {
                let record = node
                    .record(i)
                    .ok_or_else(|| anyhow!("missing leaf record {i}"))?;
                let (key, key_size) = parse_key(record)?;
                if key.parent_id != parent_cnid {
                    return Ok(out);
                }
                let data = &record[key_size..];
                if let Ok(rec) = parse_record_data(data) {
                    if matches!(rec, CatalogRecord::Folder(_) | CatalogRecord::File(_)) {
                        out.push((key, rec));
                    }
                }
            }
            let next = node.descriptor.f_link;
            if next == 0 {
                return Ok(out);
            }
            leaf_num = next;
            start_idx = 0;
        }
    }

    fn read_catalog_node(&mut self, node_num: u32) -> Result<super::btree::Node> {
        let node_size = self.catalog_header.node_size;
        let fork = self.volume_header.catalog_file;
        let block_size = self.volume_header.block_size;
        let mut reader =
            ForkReader::from_fork(&mut self.source, self.volume_offset, block_size, fork);
        read_node(&mut reader, node_num, node_size)
    }

    /// Resolve a POSIX-style path to the final catalog record.
    /// Returns `(cnid, record)`. `"/"` resolves to the root folder.
    fn resolve_path(&mut self, path: &str) -> Result<(HfsCatalogNodeID, CatalogRecord)> {
        let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();

        if components.is_empty() {
            // Root folder: look up its real record so callers get
            // accurate valence / dates instead of a synthesized stub.
            let label = self.volume_label.clone().unwrap_or_default();
            let key = CatalogKey {
                parent_id: super::types::ROOT_PARENT_ID,
                name_utf16: label.encode_utf16().collect(),
            };
            let rec = self
                .lookup_exact(&key)?
                .ok_or_else(|| anyhow!("root folder record not found"))?;
            return Ok((ROOT_FOLDER_ID, rec));
        }

        let mut current_cnid = ROOT_FOLDER_ID;
        let last_idx = components.len() - 1;
        for (i, name) in components.iter().enumerate() {
            let key = CatalogKey::from_utf8(current_cnid, name);
            let rec = self
                .lookup_exact(&key)?
                .ok_or_else(|| anyhow!("path component not found: {name:?}"))?;
            match (&rec, i == last_idx) {
                (CatalogRecord::Folder(f), false) => current_cnid = f.folder_id,
                (CatalogRecord::Folder(f), true) => return Ok((f.folder_id, rec)),
                (CatalogRecord::File(f), true) => return Ok((f.file_id, rec)),
                (CatalogRecord::File(_), false) => {
                    bail!("path component {name:?} is a file but not the last component")
                }
                _ => bail!("unexpected catalog record while resolving {name:?}: {rec:?}"),
            }
        }
        unreachable!("path resolution should have returned in the loop")
    }
}

fn record_to_stat(name: String, rec: &CatalogRecord) -> Stat {
    match rec {
        CatalogRecord::Folder(f) => Stat {
            name,
            size_bytes: 0,
            is_dir: true,
            modified: hfsplus_time(f.content_mod_date),
            created: hfsplus_time(f.create_date),
        },
        CatalogRecord::File(f) => Stat {
            name,
            size_bytes: f.data_fork.logical_size,
            is_dir: false,
            modified: hfsplus_time(f.content_mod_date),
            created: hfsplus_time(f.create_date),
        },
        // Thread records aren't surfaced through Stat.
        _ => Stat {
            name,
            size_bytes: 0,
            is_dir: false,
            modified: None,
            created: None,
        },
    }
}

/// Convert an HFS+ date (seconds since 1904-01-01 00:00 UTC) to a
/// SystemTime. The HFS+ epoch is 2,082,844,800 seconds before the Unix
/// epoch.
fn hfsplus_time(seconds_since_1904: u32) -> Option<std::time::SystemTime> {
    if seconds_since_1904 == 0 {
        return None;
    }
    const HFS_EPOCH_DELTA_SECS: u64 = 2_082_844_800;
    let secs = seconds_since_1904 as u64;
    if secs < HFS_EPOCH_DELTA_SECS {
        return None;
    }
    let unix = secs - HFS_EPOCH_DELTA_SECS;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix))
}

impl<S: Read + Seek + Send> MacFilesystem for Hfsplus<S> {
    fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let (cnid, rec) = self.resolve_path(path)?;
        if !matches!(rec, CatalogRecord::Folder(_)) {
            bail!("not a directory: {path}");
        }
        let children = self.list_children(cnid)?;
        Ok(children
            .into_iter()
            .map(|(key, rec)| record_to_stat(key.name(), &rec))
            .collect())
    }

    fn stat(&mut self, path: &str) -> Result<Stat> {
        let (_cnid, rec) = self.resolve_path(path)?;
        let name = path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        Ok(record_to_stat(name, &rec))
    }

    fn read_file_range(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (cnid, rec) = self.resolve_path(path)?;
        let file = match rec {
            CatalogRecord::File(f) => f,
            _ => bail!("not a file: {path}"),
        };
        let block_size = self.volume_header.block_size;
        let volume_offset = self.volume_offset;

        // Fast path: the inline 8 extents cover the entire data fork.
        // Slow path: consult the extents-overflow B-tree to build the
        // full extent list before reading.
        let extents = if file.data_fork.is_fully_inline() {
            file.data_fork
                .extents
                .iter()
                .copied()
                .take_while(|e| !e.is_empty())
                .collect()
        } else {
            self.extents_btree.resolve_full_extents(
                &mut self.source,
                volume_offset,
                block_size,
                cnid,
                FORK_TYPE_DATA,
                &file.data_fork,
            )?
        };

        let mut reader = ForkReader::with_extents(
            &mut self.source,
            volume_offset,
            block_size,
            extents,
            file.data_fork.logical_size,
        );
        reader.seek(SeekFrom::Start(offset))?;
        let mut filled = 0;
        while filled < buf.len() {
            match reader.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        Ok(filled)
    }

    fn volume_label(&self) -> Option<&str> {
        self.volume_label.as_deref()
    }
}
