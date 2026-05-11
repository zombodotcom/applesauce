//! Partition-table parsers (GPT and Apple Partition Map).
//!
//! Both are small, fixed-layout binary formats that index a disk into
//! discrete volumes. `probe()` tries GPT first (modern Intel Macs),
//! then APM (PowerPC and some early Intel Macs), returning whichever
//! it finds. Returns an empty vec when no recognized table is present.

use std::io::{self, Read, SeekFrom};

use anyhow::bail;
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};

use crate::BlockSource;

/// Assumed disk sector size. Real disks may use 4096; we'll detect that
/// later via `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`. 512 is correct for
/// almost every external USB drive and disk image we'll encounter.
pub const SECTOR_SIZE: u64 = 512;

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
    /// Which table this partition came from.
    pub scheme: PartitionScheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionScheme {
    Gpt,
    Apm,
}

impl Partition {
    /// True if this partition's type indicates a Mac filesystem we
    /// (will eventually) understand.
    pub fn is_mac_filesystem(&self) -> bool {
        matches!(
            self.type_id.as_str(),
            APPLE_HFS_PLUS_GUID
                | APPLE_APFS_CONTAINER_GUID
                | "Apple_HFS"
                | "Apple_HFSX"
                | "Apple_APFS"
                | "Apple_APFS_Container"
        )
    }
}

/// Probe for a partition table and return its partitions.
pub fn probe<S: BlockSource>(source: &mut S) -> anyhow::Result<Vec<Partition>> {
    if let Some(gpt) = try_gpt(source)? {
        return Ok(gpt);
    }
    if let Some(apm) = try_apm(source)? {
        return Ok(apm);
    }
    Ok(Vec::new())
}

// ---------------------------------------------------------------------
// GPT
// ---------------------------------------------------------------------

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Apple HFS / HFS+ partition type GUID (textual canonical form).
pub const APPLE_HFS_PLUS_GUID: &str = "48465300-0000-11AA-AA11-00306543ECAC";
/// Apple APFS container partition type GUID.
pub const APPLE_APFS_CONTAINER_GUID: &str = "7C3457EF-0000-11AA-AA11-00306543ECAC";

fn try_gpt<S: BlockSource>(source: &mut S) -> anyhow::Result<Option<Vec<Partition>>> {
    source.seek(SeekFrom::Start(SECTOR_SIZE))?;
    let mut header = [0u8; 92];
    if read_exact_or_short(source, &mut header)? < header.len() {
        return Ok(None);
    }
    if &header[0..8] != GPT_SIGNATURE {
        return Ok(None);
    }

    let mut cursor = &header[24..];
    let _my_lba = cursor.read_u64::<LittleEndian>()?;
    let _alt_lba = cursor.read_u64::<LittleEndian>()?;
    let _first_usable = cursor.read_u64::<LittleEndian>()?;
    let _last_usable = cursor.read_u64::<LittleEndian>()?;
    let mut _disk_guid = [0u8; 16];
    cursor.read_exact(&mut _disk_guid)?;
    let part_entry_lba = cursor.read_u64::<LittleEndian>()?;
    let num_entries = cursor.read_u32::<LittleEndian>()? as usize;
    let entry_size = cursor.read_u32::<LittleEndian>()? as usize;

    if entry_size < 128 || entry_size > 4096 || num_entries > 1024 {
        bail!(
            "GPT header looks corrupt: entry_size={entry_size}, num_entries={num_entries}"
        );
    }

    source.seek(SeekFrom::Start(part_entry_lba * SECTOR_SIZE))?;
    let mut buf = vec![0u8; entry_size];
    let mut out = Vec::new();

    for _ in 0..num_entries {
        if read_exact_or_short(source, &mut buf)? < buf.len() {
            break;
        }

        // First 16 bytes all-zero => empty slot.
        if buf[0..16].iter().all(|&b| b == 0) {
            continue;
        }

        let type_guid = format_mixed_endian_guid(&buf[0..16]);
        let start_lba = (&buf[32..40]).read_u64::<LittleEndian>()?;
        let end_lba = (&buf[40..48]).read_u64::<LittleEndian>()?;

        // Name: UTF-16LE, up to 72 bytes (36 code units), null-terminated.
        let mut name_units = Vec::with_capacity(36);
        for chunk in buf[56..56 + 72].chunks_exact(2) {
            let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
            if unit == 0 {
                break;
            }
            name_units.push(unit);
        }
        let name = String::from_utf16_lossy(&name_units);

        out.push(Partition {
            name,
            type_id: type_guid,
            start_byte: start_lba * SECTOR_SIZE,
            length_bytes: (end_lba.saturating_sub(start_lba) + 1) * SECTOR_SIZE,
            scheme: PartitionScheme::Gpt,
        });
    }

    Ok(Some(out))
}

/// Format 16 raw bytes as the canonical GUID string. The first three
/// fields are little-endian on disk; the last two are big-endian.
fn format_mixed_endian_guid(b: &[u8]) -> String {
    debug_assert_eq!(b.len(), 16);
    format!(
        "{:02X}{:02X}{:02X}{:02X}-\
         {:02X}{:02X}-\
         {:02X}{:02X}-\
         {:02X}{:02X}-\
         {:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        b[3], b[2], b[1], b[0],
        b[5], b[4],
        b[7], b[6],
        b[8], b[9],
        b[10], b[11], b[12], b[13], b[14], b[15],
    )
}

// ---------------------------------------------------------------------
// Apple Partition Map
// ---------------------------------------------------------------------

fn try_apm<S: BlockSource>(source: &mut S) -> anyhow::Result<Option<Vec<Partition>>> {
    // Block 0 is the Driver Descriptor Record. Signature "ER" (0x4552 BE).
    source.seek(SeekFrom::Start(0))?;
    let mut ddr = [0u8; 4];
    if read_exact_or_short(source, &mut ddr)? < ddr.len() {
        return Ok(None);
    }
    if &ddr[0..2] != b"ER" {
        // DDR may be optional on some hybrid disks; check block 1 anyway.
    }

    // Block 1: first partition map entry. Signature "PM" (0x504D BE).
    source.seek(SeekFrom::Start(SECTOR_SIZE))?;
    let mut first = [0u8; 512];
    if read_exact_or_short(source, &mut first)? < first.len() {
        return Ok(None);
    }
    if &first[0..2] != b"PM" {
        return Ok(None);
    }

    let total_entries = (&first[4..8]).read_u32::<BigEndian>()? as u64;
    if total_entries == 0 || total_entries > 256 {
        return Ok(None);
    }

    let mut out = Vec::new();
    for i in 1..=total_entries {
        source.seek(SeekFrom::Start(i * SECTOR_SIZE))?;
        let mut entry = [0u8; 512];
        if read_exact_or_short(source, &mut entry)? < entry.len() {
            break;
        }
        if &entry[0..2] != b"PM" {
            break;
        }

        let start_block = (&entry[8..12]).read_u32::<BigEndian>()? as u64;
        let block_count = (&entry[12..16]).read_u32::<BigEndian>()? as u64;
        let name = read_apm_string(&entry[16..16 + 32]);
        let type_id = read_apm_string(&entry[48..48 + 32]);

        // Skip the map self-entry and free space entries.
        if type_id == "Apple_partition_map" || type_id == "Apple_Free" {
            continue;
        }

        out.push(Partition {
            name,
            type_id,
            start_byte: start_block * SECTOR_SIZE,
            length_bytes: block_count * SECTOR_SIZE,
            scheme: PartitionScheme::Apm,
        });
    }

    Ok(Some(out))
}

fn read_apm_string(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Read up to `buf.len()` bytes. Returns however many were actually
/// read; callers check for short reads to detect truncated sources.
fn read_exact_or_short<S: Read>(source: &mut S, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match source.read(&mut buf[filled..]) {
            Ok(0) => return Ok(filled),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct VecSource(Cursor<Vec<u8>>);
    impl Read for VecSource {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            self.0.read(b)
        }
    }
    impl Seek for VecSource {
        fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
            self.0.seek(p)
        }
    }
    impl BlockSource for VecSource {
        fn len_bytes(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
    }

    #[test]
    fn empty_disk_no_partitions() {
        let mut src = VecSource(Cursor::new(vec![0u8; 8192]));
        let p = probe(&mut src).unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn mixed_endian_guid_known_value() {
        // Apple HFS+ type GUID raw bytes (mixed-endian on disk).
        // Canonical: 48465300-0000-11AA-AA11-00306543ECAC
        let raw = [
            0x00, 0x53, 0x46, 0x48, // 48465300 (LE: first 4 bytes reversed)
            0x00, 0x00,             // 0000
            0xAA, 0x11,             // 11AA (LE: 2 bytes reversed)
            0xAA, 0x11,             // AA11 (BE)
            0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC, // 00306543ECAC (BE)
        ];
        assert_eq!(format_mixed_endian_guid(&raw), APPLE_HFS_PLUS_GUID);
    }

    #[test]
    fn synthesized_gpt_one_hfsplus_partition() {
        // Hand-craft a minimal valid GPT layout: protective MBR at LBA 0
        // (we don't validate it), GPT header at LBA 1, one entry at LBA 2.
        let mut disk = vec![0u8; SECTOR_SIZE as usize * 64];

        // Header at LBA 1.
        let hdr = &mut disk[SECTOR_SIZE as usize..SECTOR_SIZE as usize + 92];
        hdr[0..8].copy_from_slice(GPT_SIGNATURE);
        // partition_entry_lba @ offset 72, u64 LE = 2
        hdr[72..80].copy_from_slice(&2u64.to_le_bytes());
        // num_partition_entries @ offset 80, u32 LE = 1
        hdr[80..84].copy_from_slice(&1u32.to_le_bytes());
        // size_of_partition_entry @ offset 84, u32 LE = 128
        hdr[84..88].copy_from_slice(&128u32.to_le_bytes());

        // Entry at LBA 2.
        let entry = &mut disk[(2 * SECTOR_SIZE) as usize..(2 * SECTOR_SIZE) as usize + 128];
        // Type GUID raw bytes (Apple HFS+):
        entry[0..16].copy_from_slice(&[
            0x00, 0x53, 0x46, 0x48,
            0x00, 0x00,
            0xAA, 0x11,
            0xAA, 0x11,
            0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
        ]);
        // start LBA = 40, end LBA = 99 (inclusive)
        entry[32..40].copy_from_slice(&40u64.to_le_bytes());
        entry[40..48].copy_from_slice(&99u64.to_le_bytes());
        // Name: "Test HFS+" UTF-16LE
        let name = "Test HFS+";
        for (i, c) in name.encode_utf16().enumerate() {
            entry[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }

        let mut src = VecSource(Cursor::new(disk));
        let parts = probe(&mut src).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "Test HFS+");
        assert_eq!(parts[0].type_id, APPLE_HFS_PLUS_GUID);
        assert_eq!(parts[0].start_byte, 40 * SECTOR_SIZE);
        assert_eq!(parts[0].length_bytes, 60 * SECTOR_SIZE); // 99-40+1 = 60
        assert!(parts[0].is_mac_filesystem());
    }

    #[test]
    fn synthesized_apm_one_partition() {
        let mut disk = vec![0u8; SECTOR_SIZE as usize * 8];

        // Block 0: DDR — signature 'ER'
        disk[0..2].copy_from_slice(b"ER");

        // Block 1: first PM entry (the map's self entry).
        let pm1 = &mut disk[SECTOR_SIZE as usize..SECTOR_SIZE as usize + 512];
        pm1[0..2].copy_from_slice(b"PM");
        pm1[4..8].copy_from_slice(&2u32.to_be_bytes()); // 2 entries total
        pm1[8..12].copy_from_slice(&1u32.to_be_bytes()); // start block
        pm1[12..16].copy_from_slice(&1u32.to_be_bytes()); // block count
        pm1[16..16 + b"Apple".len()].copy_from_slice(b"Apple");
        pm1[48..48 + b"Apple_partition_map".len()].copy_from_slice(b"Apple_partition_map");

        // Block 2: second PM entry (the HFS one).
        let pm2 = &mut disk[2 * SECTOR_SIZE as usize..2 * SECTOR_SIZE as usize + 512];
        pm2[0..2].copy_from_slice(b"PM");
        pm2[4..8].copy_from_slice(&2u32.to_be_bytes());
        pm2[8..12].copy_from_slice(&4u32.to_be_bytes()); // start block 4
        pm2[12..16].copy_from_slice(&3u32.to_be_bytes()); // 3 blocks
        pm2[16..16 + b"MacintoshHD".len()].copy_from_slice(b"MacintoshHD");
        pm2[48..48 + b"Apple_HFS".len()].copy_from_slice(b"Apple_HFS");

        let mut src = VecSource(Cursor::new(disk));
        let parts = probe(&mut src).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "MacintoshHD");
        assert_eq!(parts[0].type_id, "Apple_HFS");
        assert_eq!(parts[0].start_byte, 4 * SECTOR_SIZE);
        assert_eq!(parts[0].length_bytes, 3 * SECTOR_SIZE);
        assert!(parts[0].is_mac_filesystem());
    }
}
