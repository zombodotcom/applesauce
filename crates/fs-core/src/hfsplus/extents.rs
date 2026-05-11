//! HFS+ extents-overflow B-tree.
//!
//! Catalog file records carry the first 8 extents inline. Files whose
//! data is more fragmented spill the rest into this B-tree. Keys are
//! `(file_id, fork_type, start_block_in_fork)`; records hold the next
//! 8 extent descriptors starting at `start_block_in_fork`.
//!
//! See Apple Technote 1150 §"Extents Overflow File".

use std::cmp::Ordering;
use std::io::{Read, Seek};

use anyhow::{anyhow, bail, Result};

use super::btree::{read_btree_header, read_node, BTreeHeaderRecord, NodeKind};
use super::fork::{HFSPlusExtentDescriptor, HFSPlusForkData};
use super::fork_reader::ForkReader;
use super::types::HfsCatalogNodeID;

/// Data-fork marker in extents-overflow keys.
pub const FORK_TYPE_DATA: u8 = 0x00;
/// Resource-fork marker in extents-overflow keys.
pub const FORK_TYPE_RESOURCE: u8 = 0xFF;

#[derive(Debug, Clone, Copy)]
struct ExtentsKey {
    fork_type: u8,
    file_id: HfsCatalogNodeID,
    start_block: u32,
}

impl ExtentsKey {
    fn cmp_to(&self, other: &Self) -> Ordering {
        self.file_id
            .cmp(&other.file_id)
            .then(self.fork_type.cmp(&other.fork_type))
            .then(self.start_block.cmp(&other.start_block))
    }
}

/// Parse an extents-overflow key. Returns the key and the total bytes
/// consumed (key_length + 2 for the length-prefix u16).
fn parse_key(bytes: &[u8]) -> Result<(ExtentsKey, usize)> {
    if bytes.len() < 12 {
        bail!("extents key buffer too small: {} bytes", bytes.len());
    }
    let key_length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let total = 2 + key_length;
    if total > bytes.len() || key_length < 10 {
        bail!(
            "extents key length {} out of range (buffer is {} bytes)",
            key_length,
            bytes.len()
        );
    }
    let fork_type = bytes[2];
    // bytes[3] is padding (must be 0)
    let file_id = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let start_block = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    Ok((
        ExtentsKey {
            fork_type,
            file_id,
            start_block,
        },
        total,
    ))
}

/// Parse 8 extent descriptors out of a record body.
fn parse_record(data: &[u8]) -> Result<[HFSPlusExtentDescriptor; 8]> {
    if data.len() < 64 {
        bail!("extents record too small: {} bytes", data.len());
    }
    let mut out = [HFSPlusExtentDescriptor {
        start_block: 0,
        block_count: 0,
    }; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        let off = i * 8;
        let sb = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        let bc = u32::from_be_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]);
        *slot = HFSPlusExtentDescriptor {
            start_block: sb,
            block_count: bc,
        };
    }
    Ok(out)
}

/// Lazy reader over the extents-overflow B-tree. Holds the header
/// (root node, node size) plus the fork that backs the tree, so
/// lookups can re-open a ForkReader on demand.
pub struct ExtentsBTree {
    header: BTreeHeaderRecord,
    fork: HFSPlusForkData,
}

impl ExtentsBTree {
    pub fn open<S: Read + Seek>(
        source: &mut S,
        volume_offset: u64,
        block_size: u32,
        fork: HFSPlusForkData,
    ) -> Result<Self> {
        let header = {
            let mut reader = ForkReader::from_fork(source, volume_offset, block_size, fork);
            let (_desc, hdr) = read_btree_header(&mut reader)?;
            hdr
        };
        Ok(Self { header, fork })
    }

    /// Walk the tree and return every overflow record for `(file_id,
    /// fork_type)`, ordered by `start_block`.
    fn collect_overflow_records<S: Read + Seek>(
        &self,
        source: &mut S,
        volume_offset: u64,
        block_size: u32,
        file_id: HfsCatalogNodeID,
        fork_type: u8,
    ) -> Result<Vec<(u32, [HFSPlusExtentDescriptor; 8])>> {
        let target = ExtentsKey {
            fork_type,
            file_id,
            start_block: 0,
        };
        let (mut leaf_num, mut start_idx) =
            self.descend_to_leaf(source, volume_offset, block_size, &target)?;

        let mut out = Vec::new();
        loop {
            let node = self.read_overflow_node(source, volume_offset, block_size, leaf_num)?;
            for i in start_idx..node.num_records() {
                let rec = node
                    .record(i)
                    .ok_or_else(|| anyhow!("missing leaf record {i} in node {leaf_num}"))?;
                let (key, ksize) = parse_key(rec)?;
                if key.file_id != file_id || key.fork_type != fork_type {
                    return Ok(out);
                }
                let descs = parse_record(&rec[ksize..])?;
                out.push((key.start_block, descs));
            }
            let next = node.descriptor.f_link;
            if next == 0 {
                return Ok(out);
            }
            leaf_num = next;
            start_idx = 0;
        }
    }

    fn descend_to_leaf<S: Read + Seek>(
        &self,
        source: &mut S,
        volume_offset: u64,
        block_size: u32,
        target: &ExtentsKey,
    ) -> Result<(u32, usize)> {
        let mut node_num = self.header.root_node;
        for _ in 0..64 {
            let node = self.read_overflow_node(source, volume_offset, block_size, node_num)?;
            match node.descriptor.kind_enum() {
                Some(NodeKind::Index) => {
                    let mut chosen: Option<u32> = None;
                    for i in 0..node.num_records() {
                        let rec = node
                            .record(i)
                            .ok_or_else(|| anyhow!("missing index record {i}"))?;
                        let (k, ksize) = parse_key(rec)?;
                        match k.cmp_to(target) {
                            Ordering::Less | Ordering::Equal => {
                                let b = rec
                                    .get(ksize..ksize + 4)
                                    .ok_or_else(|| anyhow!("short index record"))?;
                                chosen = Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
                            }
                            Ordering::Greater => break,
                        }
                    }
                    node_num = match chosen {
                        Some(c) => c,
                        None => {
                            let first =
                                node.record(0).ok_or_else(|| anyhow!("empty index node"))?;
                            let (_, ksize) = parse_key(first)?;
                            let b = first
                                .get(ksize..ksize + 4)
                                .ok_or_else(|| anyhow!("short index record"))?;
                            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
                        }
                    };
                }
                Some(NodeKind::Leaf) => {
                    for i in 0..node.num_records() {
                        let rec = node
                            .record(i)
                            .ok_or_else(|| anyhow!("missing leaf record {i}"))?;
                        let (k, _) = parse_key(rec)?;
                        if !matches!(k.cmp_to(target), Ordering::Less) {
                            return Ok((node_num, i));
                        }
                    }
                    let next = node.descriptor.f_link;
                    if next == 0 {
                        return Ok((node_num, node.num_records()));
                    }
                    return Ok((next, 0));
                }
                other => bail!("unexpected node kind {:?} during extents descent", other),
            }
        }
        bail!("extents B-tree descent exceeded depth limit");
    }

    fn read_overflow_node<S: Read + Seek>(
        &self,
        source: &mut S,
        volume_offset: u64,
        block_size: u32,
        node_num: u32,
    ) -> Result<super::btree::Node> {
        let mut reader = ForkReader::from_fork(source, volume_offset, block_size, self.fork);
        read_node(&mut reader, node_num, self.header.node_size)
    }

    /// Given a file's inline fork data, return the full ordered list of
    /// extents (inline + overflow). Trailing empty descriptors are
    /// dropped. Errors out if the overflow chain leaves a gap.
    pub fn resolve_full_extents<S: Read + Seek>(
        &self,
        source: &mut S,
        volume_offset: u64,
        block_size: u32,
        file_id: HfsCatalogNodeID,
        fork_type: u8,
        inline: &HFSPlusForkData,
    ) -> Result<Vec<HFSPlusExtentDescriptor>> {
        let mut out: Vec<HFSPlusExtentDescriptor> = inline
            .extents
            .iter()
            .copied()
            .take_while(|e| !e.is_empty())
            .collect();
        let mut blocks: u32 = out.iter().map(|e| e.block_count).sum();

        if blocks >= inline.total_blocks {
            return Ok(out);
        }

        let records =
            self.collect_overflow_records(source, volume_offset, block_size, file_id, fork_type)?;

        for (record_start, descs) in records {
            if record_start != blocks {
                bail!(
                    "extents overflow gap for file {file_id}: expected start_block {blocks}, \
                     overflow record begins at {record_start}"
                );
            }
            for e in descs {
                if e.is_empty() {
                    break;
                }
                out.push(e);
                blocks = blocks.saturating_add(e.block_count);
            }
            if blocks >= inline.total_blocks {
                break;
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extents_key() {
        // key_length=10, fork=0xff, pad=0, file_id=0x12345678, start=0x0000_0040
        let bytes: [u8; 12] = [
            0x00, 0x0A, // key_length
            0xFF, 0x00, // fork=resource, pad
            0x12, 0x34, 0x56, 0x78, // file_id
            0x00, 0x00, 0x00, 0x40, // start_block = 64
        ];
        let (k, used) = parse_key(&bytes).unwrap();
        assert_eq!(used, 12);
        assert_eq!(k.fork_type, FORK_TYPE_RESOURCE);
        assert_eq!(k.file_id, 0x12345678);
        assert_eq!(k.start_block, 64);
    }

    #[test]
    fn key_ordering_by_file_then_fork_then_start() {
        let a = ExtentsKey {
            fork_type: 0,
            file_id: 5,
            start_block: 0,
        };
        let b = ExtentsKey {
            fork_type: 0,
            file_id: 5,
            start_block: 100,
        };
        let c = ExtentsKey {
            fork_type: 0xff,
            file_id: 5,
            start_block: 0,
        };
        let d = ExtentsKey {
            fork_type: 0,
            file_id: 6,
            start_block: 0,
        };
        assert_eq!(a.cmp_to(&b), Ordering::Less);
        assert_eq!(a.cmp_to(&c), Ordering::Less);
        assert_eq!(a.cmp_to(&d), Ordering::Less);
        assert_eq!(b.cmp_to(&c), Ordering::Less);
    }

    #[test]
    fn parses_extents_record() {
        let mut data = [0u8; 64];
        // Two non-empty extents, six empty.
        data[0..4].copy_from_slice(&100u32.to_be_bytes());
        data[4..8].copy_from_slice(&7u32.to_be_bytes());
        data[8..12].copy_from_slice(&200u32.to_be_bytes());
        data[12..16].copy_from_slice(&3u32.to_be_bytes());
        let r = parse_record(&data).unwrap();
        assert_eq!(r[0].start_block, 100);
        assert_eq!(r[0].block_count, 7);
        assert_eq!(r[1].start_block, 200);
        assert_eq!(r[1].block_count, 3);
        assert!(r[2].is_empty());
    }
}
