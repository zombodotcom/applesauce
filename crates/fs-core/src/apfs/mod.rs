//! APFS (Apple File System) read-only implementation.
//!
//! Spec reference: "Apple File System Reference" (Apple).
//! Algorithm reference (license-clean, for understanding only):
//! `sgan81/apfs-fuse` (C++, GPL).
//!
//! ## Containers vs. volumes
//!
//! Unlike HFS+ (one partition = one volume), an APFS *container*
//! occupies the whole GPT partition and holds multiple *volumes* that
//! share its free space. So the entry point is [`ApfsContainer::open`],
//! which scans the checkpoint area for the live container superblock;
//! [`ApfsContainer::volumes`] then enumerates the volumes inside.
//!
//! ## On-disk layout
//!
//! ```text
//!   Container superblock (NXSB)  — block 0 + checkpoint descriptor ring
//!     ├─ container object map (OMAP B-tree): volume oid → block
//!     └─ nx_fs_oid[]: virtual oids of the volume superblocks
//!          Volume superblock (APSB)  × N
//!            ├─ apfs_volname, role, counts, encryption flags
//!            ├─ apfs_omap_oid → volume object map        (M2)
//!            └─ apfs_root_tree_oid → file-system B-tree   (M2)
//! ```
//!
//! Everything is little-endian.
//!
//! ## Status
//!
//! - **M1 (this module set):** container + volume enumeration.
//! - M2+: per-volume directory listing, file reads, decmpfs, encryption
//!   reporting. See `.claude/plans/apfs-reader.md`.

pub mod btree;
pub mod container;
pub mod decmpfs;
pub mod filesystem;
pub mod jrecords;
pub mod object;
pub mod omap;
pub mod types;
pub mod volume;

pub use container::{ApfsContainer, NxSuperblock};
pub use filesystem::ApfsVolume;
pub use volume::ApfsVolumeInfo;
