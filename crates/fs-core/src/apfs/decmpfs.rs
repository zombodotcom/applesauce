//! decmpfs transparent compression.
//!
//! macOS stores many files compressed behind a `com.apple.decmpfs`
//! extended attribute. The attribute value begins with a 16-byte header
//! (`decmpfs_disk_header`): a magic, a compression type, and the
//! logical (uncompressed) size. Where the *compressed* bytes live
//! depends on the type:
//!
//! - **odd types** (3, 7, 11) keep the payload inline, right after the
//!   header in the decmpfs attribute;
//! - **even types** (4, 8, 12) keep it in a `com.apple.ResourceFork`
//!   attribute laid out in ≤64 KiB chunks.
//!
//! Codecs by type: 3/4 = zlib, 7/8 = LZVN, 11/12 = LZFSE; 1 = stored
//! uncompressed. zlib streams (and zlib chunks) use a `0xFF` first byte
//! to mean "stored literally, not deflated".

use anyhow::{bail, Result};

/// `decmpfs` magic: the bytes `fpmc` read little-endian.
pub const DECMPFS_MAGIC: u32 = 0x636d_7066;
/// Header length preceding inline payloads.
pub const DECMPFS_HEADER_LEN: usize = 16;
/// Each resource-fork chunk decompresses to at most this many bytes.
pub const DECMPFS_CHUNK: usize = 65536;

/// Parsed `decmpfs_disk_header`.
#[derive(Debug, Clone, Copy)]
pub struct DecmpfsHeader {
    pub compression_type: u32,
    pub uncompressed_size: u64,
}

impl DecmpfsHeader {
    /// Parse the 16-byte header from the start of a decmpfs attribute.
    pub fn parse(attr: &[u8]) -> Result<Self> {
        if attr.len() < DECMPFS_HEADER_LEN {
            bail!("decmpfs attribute too short ({} bytes)", attr.len());
        }
        let magic = u32::from_le_bytes(attr[0..4].try_into().unwrap());
        if magic != DECMPFS_MAGIC {
            bail!("bad decmpfs magic 0x{magic:08X}");
        }
        Ok(Self {
            compression_type: u32::from_le_bytes(attr[4..8].try_into().unwrap()),
            uncompressed_size: u64::from_le_bytes(attr[8..16].try_into().unwrap()),
        })
    }

    /// True when the compressed payload is stored in the resource fork
    /// rather than inline in the decmpfs attribute.
    pub fn is_resource_fork(&self) -> bool {
        matches!(self.compression_type, 4 | 8 | 12)
    }
}

/// Decompress an *inline* decmpfs payload (the bytes after the 16-byte
/// header), given the compression type and expected output size.
pub fn decompress_inline(
    compression_type: u32,
    payload: &[u8],
    uncompressed_size: u64,
) -> Result<Vec<u8>> {
    match compression_type {
        // Stored uncompressed in the attribute.
        1 => Ok(payload[..(uncompressed_size as usize).min(payload.len())].to_vec()),
        // zlib in the attribute.
        3 => zlib_block(payload, uncompressed_size as usize),
        // LZVN in the attribute.
        7 => lzvn_block(payload, uncompressed_size as usize),
        // LZFSE in the attribute.
        11 => lzfse_block(payload, uncompressed_size as usize),
        other => bail!("unsupported inline decmpfs compression type {other}"),
    }
}

/// Decompress a resource-fork payload (the bytes of the
/// `com.apple.ResourceFork` attribute) into the full file.
pub fn decompress_resource_fork(
    compression_type: u32,
    rsrc: &[u8],
    uncompressed_size: u64,
) -> Result<Vec<u8>> {
    match compression_type {
        4 => zlib_resource_fork(rsrc, uncompressed_size as usize),
        12 => lzfse_resource_fork(rsrc, uncompressed_size as usize),
        8 => bail!("LZVN resource-fork compression (type 8) not yet supported"),
        other => bail!("unsupported resource-fork decmpfs compression type {other}"),
    }
}

/// Decompress one zlib block. A leading `0xFF` byte means the remaining
/// bytes are stored literally (Apple's escape for incompressible data).
fn zlib_block(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    if data.first() == Some(&0xFF) {
        return Ok(data[1..].to_vec());
    }
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut out = Vec::with_capacity(expected);
    ZlibDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Decompress one LZFSE block via the pure-Rust `lzfse_rust` crate.
fn lzfse_block(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    // decode_bytes appends decoded output to the Vec.
    let mut out = Vec::with_capacity(expected);
    lzfse_rust::decode_bytes(data, &mut out).map_err(|e| anyhow::anyhow!("lzfse decode: {e:?}"))?;
    Ok(out)
}

/// LZVN decode. No standalone Rust LZVN decoder is available yet, so
/// type 7/8 files are reported clearly rather than read as garbage.
fn lzvn_block(_data: &[u8], _expected: usize) -> Result<Vec<u8>> {
    bail!("LZVN decmpfs compression (types 7/8) not yet supported")
}

/// zlib resource-fork format (HFS+ `cmpf`): a 16-byte big-endian
/// resource header, then at `dataOffset` a u32-BE block-table length
/// followed by a little-endian block table — `num_blocks` then
/// `(offset, len)` pairs relative to the table start — each block a
/// [`zlib_block`] producing up to [`DECMPFS_CHUNK`] bytes.
fn zlib_resource_fork(rsrc: &[u8], expected: usize) -> Result<Vec<u8>> {
    if rsrc.len() < 16 {
        bail!("zlib resource fork too short");
    }
    let data_offset = u32::from_be_bytes(rsrc[0..4].try_into().unwrap()) as usize;
    // The block table sits at data_offset + 4 (after a u32-BE total len).
    let table = data_offset
        .checked_add(4)
        .filter(|&t| t + 4 <= rsrc.len())
        .ok_or_else(|| anyhow::anyhow!("zlib resource fork: bad data offset"))?;
    let num_blocks = u32::from_le_bytes(rsrc[table..table + 4].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(expected);
    for i in 0..num_blocks {
        let ent = table + 4 + i * 8;
        let off = u32::from_le_bytes(
            rsrc.get(ent..ent + 4)
                .ok_or_else(short)?
                .try_into()
                .unwrap(),
        ) as usize;
        let len = u32::from_le_bytes(
            rsrc.get(ent + 4..ent + 8)
                .ok_or_else(short)?
                .try_into()
                .unwrap(),
        ) as usize;
        // Block offsets are relative to the start of the block table.
        let start = table + off;
        let block = rsrc.get(start..start + len).ok_or_else(short)?;
        out.extend_from_slice(&zlib_block(block, DECMPFS_CHUNK)?);
    }
    out.truncate(expected);
    Ok(out)
}

/// LZFSE resource-fork format: a little-endian table of `u32` chunk
/// end-offsets (Apple's "blockmap"): `num_chunks`, then `num_chunks`
/// cumulative offsets into the payload, each chunk an [`lzfse_block`].
fn lzfse_resource_fork(rsrc: &[u8], expected: usize) -> Result<Vec<u8>> {
    if rsrc.len() < 4 {
        bail!("lzfse resource fork too short");
    }
    let num_chunks = u32::from_le_bytes(rsrc[0..4].try_into().unwrap()) as usize;
    // Header: num_chunks followed by num_chunks cumulative end offsets.
    let header_len = 4 + num_chunks * 4;
    if rsrc.len() < header_len {
        bail!("lzfse resource fork: truncated chunk table");
    }
    let mut out = Vec::with_capacity(expected);
    let mut prev = header_len;
    for i in 0..num_chunks {
        let end = u32::from_le_bytes(rsrc[4 + i * 4..8 + i * 4].try_into().unwrap()) as usize;
        let block = rsrc.get(prev..end).ok_or_else(short)?;
        let want = (expected - out.len()).min(DECMPFS_CHUNK);
        out.extend_from_slice(&lzfse_block(block, want)?);
        prev = end;
    }
    out.truncate(expected);
    Ok(out)
}

fn short() -> anyhow::Error {
    anyhow::anyhow!("decmpfs resource fork: truncated block")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn make_header(comp_type: u32, size: u64) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&DECMPFS_MAGIC.to_le_bytes());
        h.extend_from_slice(&comp_type.to_le_bytes());
        h.extend_from_slice(&size.to_le_bytes());
        h
    }

    #[test]
    fn header_parses_and_routes() {
        let h = DecmpfsHeader::parse(&make_header(4, 1234)).unwrap();
        assert_eq!(h.compression_type, 4);
        assert_eq!(h.uncompressed_size, 1234);
        assert!(h.is_resource_fork());
        assert!(!DecmpfsHeader::parse(&make_header(3, 1))
            .unwrap()
            .is_resource_fork());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut h = make_header(3, 1);
        h[0] ^= 0xff;
        assert!(DecmpfsHeader::parse(&h).is_err());
    }

    #[test]
    fn inline_zlib_round_trips() {
        let data = b"the quick brown fox jumps over the lazy dog".repeat(10);
        let comp = zlib_compress(&data);
        let out = decompress_inline(3, &comp, data.len() as u64).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn inline_zlib_literal_escape() {
        // 0xFF prefix → stored literally.
        let data = b"incompressible-ish";
        let mut payload = vec![0xFF];
        payload.extend_from_slice(data);
        let out = decompress_inline(3, &payload, data.len() as u64).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn inline_uncompressed_type1() {
        let data = b"plain stored bytes";
        let out = decompress_inline(1, data, data.len() as u64).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn inline_lzfse_round_trips() {
        let data = b"LZFSE test payload, repeated for compressibility. ".repeat(20);
        let mut comp = Vec::new();
        lzfse_rust::encode_bytes(&data, &mut comp).unwrap();
        let out = decompress_inline(11, &comp, data.len() as u64).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn lzvn_reports_unsupported() {
        assert!(decompress_inline(7, b"whatever", 8).is_err());
    }

    #[test]
    fn zlib_resource_fork_round_trips() {
        // Build a two-chunk zlib resource fork by hand.
        let chunk0 = b"first chunk of data ".repeat(100);
        let chunk1 = b"second chunk of data ".repeat(50);
        let c0 = zlib_compress(&chunk0);
        let c1 = zlib_compress(&chunk1);

        // Block table starts at data_offset+4; offsets are relative to it.
        let data_offset = 16usize;
        let table_start = data_offset + 4; // = 20
        let num_blocks = 2usize;
        let entries_len = 4 + num_blocks * 8; // table: count + 2*(off,len)
        let b0_off = entries_len;
        let b1_off = b0_off + c0.len();

        let mut rsrc = vec![0u8; table_start];
        rsrc[0..4].copy_from_slice(&(data_offset as u32).to_be_bytes());
        // table
        rsrc.extend_from_slice(&(num_blocks as u32).to_le_bytes());
        rsrc.extend_from_slice(&(b0_off as u32).to_le_bytes());
        rsrc.extend_from_slice(&(c0.len() as u32).to_le_bytes());
        rsrc.extend_from_slice(&(b1_off as u32).to_le_bytes());
        rsrc.extend_from_slice(&(c1.len() as u32).to_le_bytes());
        rsrc.extend_from_slice(&c0);
        rsrc.extend_from_slice(&c1);

        let mut expected = chunk0.clone();
        expected.extend_from_slice(&chunk1);
        let out = decompress_resource_fork(4, &rsrc, expected.len() as u64).unwrap();
        assert_eq!(out, expected);
    }
}
