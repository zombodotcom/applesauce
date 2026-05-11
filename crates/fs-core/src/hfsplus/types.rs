//! Shared HFS+ types and constants.

/// Catalog Node ID — HFS+'s persistent identifier for every file and
/// folder. Root parent has CNID 1; the root folder of the volume is
/// CNID 2.
pub type HfsCatalogNodeID = u32;

/// CNID of the parent of the root folder. Threads pointing at this
/// identify the root.
pub const ROOT_PARENT_ID: HfsCatalogNodeID = 1;
/// CNID of the root folder of any HFS+ volume.
pub const ROOT_FOLDER_ID: HfsCatalogNodeID = 2;
/// CNID of the Extents Overflow file.
pub const EXTENTS_FILE_ID: HfsCatalogNodeID = 3;
/// CNID of the Catalog file.
pub const CATALOG_FILE_ID: HfsCatalogNodeID = 4;
/// CNID of the Allocation file.
pub const ALLOCATION_FILE_ID: HfsCatalogNodeID = 6;
/// CNID of the Attributes B-tree file.
pub const ATTRIBUTES_FILE_ID: HfsCatalogNodeID = 8;

/// "H+" — case-insensitive HFS+ volume signature.
pub const HFSPLUS_SIGNATURE: u16 = 0x482B; // big-endian 'H' 'P' -> wait, 'H'=0x48, '+'=0x2B
/// "HX" — case-sensitive HFSX volume signature.
pub const HFSX_SIGNATURE: u16 = 0x4858;

/// HFS+ volume header is located at byte 1024 from the start of the volume.
pub const VOLUME_HEADER_OFFSET: u64 = 1024;
/// Size of the on-disk HFSPlusVolumeHeader structure.
pub const VOLUME_HEADER_SIZE: usize = 512;
