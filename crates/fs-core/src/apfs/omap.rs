//! APFS object map (OMAP).
//!
//! An object map translates a virtual object id (and transaction id)
//! into the physical block where that object currently lives. The
//! container's OMAP maps volume-superblock oids to their blocks; each
//! volume has its own OMAP for its file-system objects (used in M2).
//!
//! `omap_phys_t` points at a B-tree (`om_tree_oid`, a physical oid =
//! block address) whose leaves map `omap_key {oid, xid}` →
//! `omap_val {flags, size, paddr}`. Keys are ordered ascending by
//! `(oid, xid)`.

use std::io::{Read, Seek};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use binrw::{BinRead, BinReaderExt};
use quick_cache::sync::Cache;

use super::btree::FixedKvNode;
use super::object::BlockReader;
use super::types::{Oid, Paddr, Xid};

/// Cache of parsed OMAP b-tree nodes by block address. The root and
/// index levels are re-read on every oid resolution; caching them turns
/// a cold directory walk's many redundant OMAP reads into one.
const OMAP_NODE_CACHE_CAPACITY: usize = 2048;

/// Fixed key size for an OMAP B-tree: `omap_key` = oid(8) + xid(8).
const OMAP_KEY_SIZE: usize = 16;
/// Fixed value size at a leaf: `omap_val` = flags(4) + size(4) + paddr(8).
const OMAP_VAL_SIZE: usize = 16;
/// Index-node value: a child node's physical address (oid_t).
const OMAP_INDEX_VAL_SIZE: usize = 8;

/// `omap_phys_t` — the object-map root structure.
#[derive(BinRead, Debug, Clone)]
#[brw(little)]
pub struct OmapPhys {
    #[br(pad_before = 32)] // skip obj_phys_t header
    pub flags: u32,
    pub snap_count: u32,
    pub tree_type: u32,
    pub snapshot_tree_type: u32,
    pub tree_oid: Oid,
    pub snapshot_tree_oid: Oid,
    pub most_recent_snap: Xid,
    pub pending_revert_min: Xid,
    pub pending_revert_max: Xid,
}

/// A resolved OMAP entry.
#[derive(Debug, Clone, Copy)]
pub struct OmapVal {
    pub flags: u32,
    pub size: u32,
    pub paddr: Paddr,
}

/// An opened object map: the physical address of its B-tree root plus a
/// cache of its parsed nodes.
pub struct Omap {
    tree_root: Paddr,
    node_cache: Cache<Paddr, Arc<FixedKvNode>>,
}

impl Omap {
    /// Open the OMAP whose `omap_phys_t` lives at physical block
    /// `omap_paddr` (this is `nx_omap_oid` for the container, which is
    /// a physical oid).
    pub fn open<S: Read + Seek>(
        reader: &mut BlockReader<'_, S>,
        omap_paddr: Paddr,
    ) -> Result<Self> {
        let (_hdr, block) = reader.read_object(omap_paddr)?;
        let mut cursor = std::io::Cursor::new(&block);
        let omap: OmapPhys = cursor.read_le()?;
        if omap.tree_oid == 0 {
            bail!("omap at block {omap_paddr} has no tree");
        }
        Ok(Self {
            tree_root: omap.tree_oid as Paddr,
            node_cache: Cache::new(OMAP_NODE_CACHE_CAPACITY),
        })
    }

    /// Read and parse an OMAP b-tree node at `paddr`, consulting the
    /// node cache (the root + index levels recur across every lookup).
    fn node<S: Read + Seek>(
        &self,
        reader: &mut BlockReader<'_, S>,
        paddr: Paddr,
    ) -> Result<Arc<FixedKvNode>> {
        if let Some(n) = self.node_cache.get(&paddr) {
            return Ok(n);
        }
        let block = reader.read_block(paddr)?;
        let node = Arc::new(FixedKvNode::parse(block)?);
        if !node.header.is_fixed_kv() {
            bail!("omap b-tree node is not fixed-kv");
        }
        self.node_cache.insert(paddr, node.clone());
        Ok(node)
    }

    /// Resolve `oid` to its physical block, picking the entry with the
    /// greatest `xid` not exceeding `max_xid` (use [`Xid::MAX`] for the
    /// latest). Returns `None` if the oid isn't mapped.
    pub fn lookup<S: Read + Seek>(
        &self,
        reader: &mut BlockReader<'_, S>,
        oid: Oid,
        max_xid: Xid,
    ) -> Result<Option<OmapVal>> {
        let mut node_paddr = self.tree_root;

        // Bounded descent guards against a corrupt/cyclic tree.
        for _ in 0..32 {
            let node = self.node(reader, node_paddr)?;

            if node.header.is_leaf() {
                return Ok(self.leaf_lookup(&node, oid, max_xid));
            }

            // Index node: follow the child for the greatest key <= target.
            let mut chosen: Option<Paddr> = None;
            for i in 0..node.nkeys() {
                let key = node
                    .key(i, OMAP_KEY_SIZE)
                    .ok_or_else(|| anyhow!("short omap index key {i}"))?;
                let (k_oid, k_xid) = parse_omap_key(key);
                if key_le_target(k_oid, k_xid, oid, max_xid) {
                    let val = node
                        .value(i, OMAP_INDEX_VAL_SIZE)
                        .ok_or_else(|| anyhow!("short omap index val {i}"))?;
                    let child = u64::from_le_bytes(val.try_into().unwrap());
                    chosen = Some(child as Paddr);
                } else {
                    break;
                }
            }
            match chosen {
                Some(child) => node_paddr = child,
                None => return Ok(None),
            }
        }
        bail!("omap descent exceeded depth limit (corrupt tree?)");
    }

    fn leaf_lookup(&self, node: &FixedKvNode, oid: Oid, max_xid: Xid) -> Option<OmapVal> {
        let mut best: Option<(Xid, OmapVal)> = None;
        for i in 0..node.nkeys() {
            let key = node.key(i, OMAP_KEY_SIZE)?;
            let (k_oid, k_xid) = parse_omap_key(key);
            if k_oid != oid || k_xid > max_xid {
                continue;
            }
            let val = node.value(i, OMAP_VAL_SIZE)?;
            let flags = u32::from_le_bytes(val[0..4].try_into().unwrap());
            let size = u32::from_le_bytes(val[4..8].try_into().unwrap());
            let paddr = i64::from_le_bytes(val[8..16].try_into().unwrap());
            let ov = OmapVal { flags, size, paddr };
            match best {
                Some((bx, _)) if bx >= k_xid => {}
                _ => best = Some((k_xid, ov)),
            }
        }
        best.map(|(_, v)| v)
    }
}

fn parse_omap_key(key: &[u8]) -> (Oid, Xid) {
    let oid = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let xid = u64::from_le_bytes(key[8..16].try_into().unwrap());
    (oid, xid)
}

/// Ordering test used during index descent: is `(k_oid, k_xid)` <=
/// `(oid, max_xid)` under the OMAP's ascending `(oid, xid)` order?
fn key_le_target(k_oid: Oid, k_xid: Xid, oid: Oid, max_xid: Xid) -> bool {
    (k_oid, k_xid) <= (oid, max_xid)
}
