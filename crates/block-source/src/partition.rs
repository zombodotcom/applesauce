//! Partition-table parsers (GPT, Apple Partition Map).
//!
//! Stubbed for the initial scaffold. Implementation follows in the
//! "GPT + APM partition parsers" task.

use crate::BlockSource;

/// A discovered partition on a disk.
#[derive(Debug, Clone)]
pub struct Partition {
    /// Partition name from the table (UTF-8).
    pub name: String,
    /// Partition type identifier (GUID for GPT, type string for APM).
    pub type_id: String,
    /// Byte offset from the start of the source.
    pub start_byte: u64,
    /// Length in bytes.
    pub length_bytes: u64,
}

/// Probe a block source for a partition table and return its partitions.
///
/// Returns `Ok(vec![])` for sources without a recognized table — callers
/// can treat the whole source as a single volume if appropriate.
pub fn probe<S: BlockSource>(_source: &mut S) -> anyhow::Result<Vec<Partition>> {
    unimplemented!("partition::probe lands in the next commit")
}
