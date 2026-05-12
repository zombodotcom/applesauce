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
pub const HFSPLUS_SIGNATURE: u16 = 0x482B;
/// "HX" — case-sensitive HFSX volume signature.
pub const HFSX_SIGNATURE: u16 = 0x4858;
/// "BD" — classic HFS (Mac OS Standard) volume signature. We don't
/// read pure HFS, but Mac OS 8.1–10.3 wrote HFS+ volumes _wrapped_
/// inside an HFS shell, and the outer wrapper carries this signature.
pub const HFS_SIGNATURE: u16 = 0x4244;

/// HFS+ volume header is located at byte 1024 from the start of the volume.
pub const VOLUME_HEADER_OFFSET: u64 = 1024;
/// Size of the on-disk HFSPlusVolumeHeader structure.
pub const VOLUME_HEADER_SIZE: usize = 512;
