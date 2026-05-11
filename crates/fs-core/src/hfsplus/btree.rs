//! HFS+ B-tree node parsing.
//!
//! Every B-tree file (catalog, extents overflow, attributes) is built
//! out of fixed-size nodes. Node 0 is always the "header node" and
//! holds the metadata that describes the rest of the tree. Catalog and
//! extents trees share this layout — only the record key/value formats
//! differ. Higher-level modules (`catalog`, `extents_overflow`) layer
//! their record parsing on top of the primitives here.

use std::io::{Cursor, Read, Seek, SeekFrom};

use anyhow::{anyhow, bail, Result};
use binrw::{BinRead, BinReaderExt};

/// Fixed 14-byte descriptor at the start of every B-tree node.
#[derive(BinRead, Debug, Clone, Copy)]
#[brw(big)]
pub struct NodeDescriptor {
    pub f_link: u32,
    pub b_link: u32,
    pub kind: i8,
    pub height: u8,
    pub num_records: u16,
    pub _reserved: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Leaf,
    Index,
    Header,
    Map,
}

impl NodeDescriptor {
    pub fn kind_enum(&self) -> Option<NodeKind> {
        match self.kind {
            -1 => Some(NodeKind::Leaf),
            0 => Some(NodeKind::Index),
            1 => Some(NodeKind::Header),
            2 => Some(NodeKind::Map),
            _ => None,
        }
    }
}

/// The 106-byte B-tree header record, found as record 0 of node 0.
#[derive(BinRead, Debug, Clone)]
#[brw(big)]
pub struct BTreeHeaderRecord {
    pub tree_depth: u16,
    pub root_node: u32,
    pub leaf_records: u32,
    pub first_leaf_node: u32,
    pub last_leaf_node: u32,
    pub node_size: u16,
    pub max_key_length: u16,
    pub total_nodes: u32,
    pub free_nodes: u32,
    pub _reserved1: u16,
    pub clump_size: u32,
    pub btree_type: u8,
    pub key_compare_type: u8,
    pub attributes: u32,
    pub _reserved3: [u32; 16],
}

/// A parsed B-tree node: descriptor plus a snapshot of raw record bytes
/// with their offsets. Callers slice the bytes to extract typed records.
#[derive(Debug)]
pub struct Node {
    pub descriptor: NodeDescriptor,
    /// Raw node contents, exactly `node_size` bytes.
    pub bytes: Vec<u8>,
    /// Byte offsets within `bytes` for record 0..num_records. The
    /// length of each record is `record_offsets[i+1] - record_offsets[i]`.
    pub record_offsets: Vec<u16>,
}

impl Node {
    /// Number of records in this node.
    pub fn num_records(&self) -> usize {
        self.descriptor.num_records as usize
    }

    /// Get the raw bytes for record `i`. Returns `None` for out-of-range.
    pub fn record(&self, i: usize) -> Option<&[u8]> {
        if i + 1 >= self.record_offsets.len() {
            return None;
        }
        let start = self.record_offsets[i] as usize;
        let end = self.record_offsets[i + 1] as usize;
        if start > end || end > self.bytes.len() {
            return None;
        }
        Some(&self.bytes[start..end])
    }
}

/// Read node `node_num` of size `node_size` from a Read+Seek source
/// (typically a `ForkReader` over the tree's fork).
pub fn read_node<S: Read + Seek>(
    source: &mut S,
    node_num: u32,
    node_size: u16,
) -> Result<Node> {
    let node_size = node_size as usize;
    source.seek(SeekFrom::Start(node_num as u64 * node_size as u64))?;

    let mut bytes = vec![0u8; node_size];
    source.read_exact(&mut bytes)?;

    let mut head = Cursor::new(&bytes[..]);
    let descriptor: NodeDescriptor = head
        .read_be()
        .map_err(|e| anyhow!("decoding node descriptor: {e}"))?;

    let n = descriptor.num_records as usize;
    if (n + 1) * 2 > node_size {
        bail!(
            "node {} claims {} records — too many for size {}",
            node_num,
            n,
            node_size
        );
    }

    // Offsets live in the last 2*(n+1) bytes, in reverse order. We
    // re-flip them into ascending record order (record_offsets[i] is
    // the start of record i; record_offsets[n] is the start of free
    // space, used as the end of the last record).
    let mut record_offsets = Vec::with_capacity(n + 1);
    for i in 0..=n {
        let off_pos = node_size - 2 * (i + 1);
        let off = u16::from_be_bytes([bytes[off_pos], bytes[off_pos + 1]]);
        record_offsets.push(off);
    }

    Ok(Node {
        descriptor,
        bytes,
        record_offsets,
    })
}

/// Read the B-tree header record from node 0 of `source`. Returns the
/// header along with the node descriptor (so callers can sanity-check
/// kind == NodeKind::Header).
pub fn read_btree_header<S: Read + Seek>(source: &mut S) -> Result<(NodeDescriptor, BTreeHeaderRecord)> {
    // The header node has node_size = 512 minimum (or the value already
    // recorded inside the header). We have a chicken-and-egg: we need
    // node_size to know how much to read for node 0. Apple's spec
    // guarantees that node 0 is always at least 512 bytes (the minimum
    // node size). The header record starts immediately after the
    // 14-byte descriptor.
    source.seek(SeekFrom::Start(0))?;
    let mut prefix = [0u8; 14 + 106];
    source.read_exact(&mut prefix)?;

    let mut head = Cursor::new(&prefix[..]);
    let descriptor: NodeDescriptor = head
        .read_be()
        .map_err(|e| anyhow!("decoding header node descriptor: {e}"))?;
    if descriptor.kind_enum() != Some(NodeKind::Header) {
        bail!(
            "B-tree node 0 is not a header node (kind = {})",
            descriptor.kind
        );
    }

    let header: BTreeHeaderRecord = head
        .read_be()
        .map_err(|e| anyhow!("decoding B-tree header record: {e}"))?;
    Ok((descriptor, header))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a header node with one BTreeHeaderRecord (and no other
    /// records). The node is `node_size` bytes total. Record offsets:
    /// [14, 14+106, ...].
    fn build_header_node(node_size: u16) -> Vec<u8> {
        let n = node_size as usize;
        let mut buf = vec![0u8; n];

        // Descriptor: kind = 1 (header), height = 0, num_records = 3
        // (Apple header nodes have: header, user data, map records).
        buf[0..4].copy_from_slice(&0u32.to_be_bytes()); // f_link
        buf[4..8].copy_from_slice(&0u32.to_be_bytes()); // b_link
        buf[8] = 1i8 as u8; // kind = header
        buf[9] = 0; // height
        buf[10..12].copy_from_slice(&3u16.to_be_bytes()); // num_records
        buf[12..14].copy_from_slice(&0u16.to_be_bytes()); // reserved

        // Record 0: BTreeHeaderRecord, 106 bytes starting at offset 14.
        let rec = &mut buf[14..14 + 106];
        rec[0..2].copy_from_slice(&2u16.to_be_bytes()); // tree_depth
        rec[2..6].copy_from_slice(&7u32.to_be_bytes()); // root_node
        rec[6..10].copy_from_slice(&100u32.to_be_bytes()); // leaf_records
        rec[10..14].copy_from_slice(&5u32.to_be_bytes()); // first_leaf_node
        rec[14..18].copy_from_slice(&9u32.to_be_bytes()); // last_leaf_node
        rec[18..20].copy_from_slice(&node_size.to_be_bytes()); // node_size
        rec[20..22].copy_from_slice(&512u16.to_be_bytes()); // max_key_length

        // Record offsets at the end, in reverse order:
        //   offsets[0] = 14   (first record start)
        //   offsets[1] = 120  (second record start)
        //   offsets[2] = 248  (third record start; 128 bytes user data)
        //   offsets[3] = n    (free space marker = end of last record)
        let free = (n - 2 * 4) as u16; // free space lives before offsets
        let offs: [u16; 4] = [14, 120, 248, free];
        for (i, &v) in offs.iter().enumerate() {
            let pos = n - 2 * (i + 1);
            buf[pos..pos + 2].copy_from_slice(&v.to_be_bytes());
        }
        buf
    }

    #[test]
    fn parses_header_node() {
        let buf = build_header_node(512);
        let mut c = Cursor::new(buf);
        let (desc, header) = read_btree_header(&mut c).unwrap();
        assert_eq!(desc.kind_enum(), Some(NodeKind::Header));
        assert_eq!(desc.num_records, 3);
        assert_eq!(header.tree_depth, 2);
        assert_eq!(header.root_node, 7);
        assert_eq!(header.first_leaf_node, 5);
        assert_eq!(header.last_leaf_node, 9);
        assert_eq!(header.node_size, 512);
    }

    #[test]
    fn read_node_extracts_record_offsets() {
        let buf = build_header_node(512);
        let mut c = Cursor::new(buf);
        let node = read_node(&mut c, 0, 512).unwrap();
        assert_eq!(node.num_records(), 3);
        // Offset 0 should be 14 (start of header record), offset 1 = 120.
        assert_eq!(node.record_offsets[0], 14);
        assert_eq!(node.record_offsets[1], 120);
        // record(0) span = 14..120 = 106 bytes — matches BTreeHeaderRecord size.
        let r0 = node.record(0).unwrap();
        assert_eq!(r0.len(), 106);
    }

    #[test]
    fn rejects_too_many_records() {
        let mut buf = vec![0u8; 64];
        buf[10..12].copy_from_slice(&100u16.to_be_bytes()); // claim 100 records in 64 bytes
        let mut c = Cursor::new(buf);
        let err = read_node(&mut c, 0, 64).unwrap_err();
        assert!(err.to_string().contains("too many for size"));
    }
}
