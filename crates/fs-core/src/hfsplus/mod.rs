//! HFS+ (Mac OS Extended) read-only implementation.
//!
//! Spec reference: Apple Technote 1150.
//! Algorithm reference (license-clean, for understanding only):
//! `catacombae/hfsexplorer` (Java, GPL).
//!
//! On-disk layout overview:
//!
//! ```text
//!   0..1024   reserved boot blocks (zeros on modern volumes)
//!   1024..1536 HFSPlusVolumeHeader
//!   ...        allocation blocks holding special files (catalog,
//!              extents overflow, allocation, attributes) and user data
//! ```
//!
//! Every multi-byte integer is big-endian.

pub mod btree;
pub mod catalog;
pub mod filesystem;
pub mod fork;
pub mod fork_reader;
pub mod types;
pub mod volume;

pub use filesystem::Hfsplus;
pub use fork::{HFSPlusExtentDescriptor, HFSPlusForkData};
pub use types::{HfsCatalogNodeID, HFSPLUS_SIGNATURE, HFSX_SIGNATURE};
pub use volume::HFSPlusVolumeHeader;
