//! Windows physical-disk block source.
//!
//! Wraps `\\.\PhysicalDriveN`. Reads against a raw physical-disk handle
//! must be sector-aligned in both offset and length, so this type does
//! its own sector-aligned read-ahead buffering and serves arbitrary
//! `Read` requests from that cache.
//!
//! Opening a physical disk requires the process to run as
//! Administrator.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::MaybeUninit;
use std::os::windows::io::{FromRawHandle, RawHandle};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO};
use windows::Win32::System::IO::DeviceIoControl;

use crate::BlockSource;

/// Read-ahead buffer size. Multiple of any common sector size (512 / 4096).
const CACHE_SIZE: usize = 64 * 1024;

/// Summary info about a physical disk discovered by [`enumerate`].
#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub drive_number: u32,
    pub length_bytes: u64,
}

/// Probe `\\.\PhysicalDrive0..32` and return what's present.
///
/// Each candidate is opened briefly to query length, then closed. Drives
/// that fail to open (not present, or insufficient privilege) are
/// silently skipped. An empty result usually means the process is not
/// elevated.
pub fn enumerate() -> Vec<DiskInfo> {
    let mut out = Vec::new();
    for n in 0..32u32 {
        if let Ok(info) = peek(n) {
            out.push(info);
        }
    }
    out
}

fn peek(drive_number: u32) -> io::Result<DiskInfo> {
    let handle = open_handle(drive_number)?;
    let length = query_length(handle).inspect_err(|_e| {
        unsafe {
            let _ = CloseHandle(handle);
        }
    })?;
    unsafe {
        let _ = CloseHandle(handle);
    }
    Ok(DiskInfo {
        drive_number,
        length_bytes: length,
    })
}

/// Windows raw physical disk handle.
pub struct PhysicalDisk {
    inner: File,
    sector_size: u32,
    length: u64,
    pos: u64,
    cache: Vec<u8>,
    cache_start: u64,
    cache_len: usize,
}

impl PhysicalDisk {
    /// Open `\\.\PhysicalDriveN`. Requires Administrator.
    pub fn open(drive_number: u32) -> io::Result<Self> {
        let handle = open_handle(drive_number)?;
        let length = match query_length(handle) {
            Ok(l) => l,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return Err(e);
            }
        };

        // Hand ownership of the HANDLE to std::fs::File. From here on,
        // closing happens automatically when File drops.
        let raw: RawHandle = handle.0 as RawHandle;
        let inner = unsafe { File::from_raw_handle(raw) };

        Ok(Self {
            inner,
            sector_size: 512, // Conservative default. Refine via
            // IOCTL_DISK_GET_DRIVE_GEOMETRY_EX later.
            length,
            pos: 0,
            cache: vec![0; CACHE_SIZE],
            cache_start: 0,
            cache_len: 0,
        })
    }

    fn cache_covers(&self, pos: u64) -> bool {
        self.cache_len > 0
            && pos >= self.cache_start
            && pos < self.cache_start.saturating_add(self.cache_len as u64)
    }

    fn fill_cache(&mut self) -> io::Result<()> {
        let sector = self.sector_size as u64;
        let aligned = self.pos & !(sector - 1);
        self.inner.seek(SeekFrom::Start(aligned))?;

        let mut filled = 0usize;
        while filled < self.cache.len() {
            match self.inner.read(&mut self.cache[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.cache_start = aligned;
        self.cache_len = filled;
        Ok(())
    }
}

impl Read for PhysicalDisk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.length {
            return Ok(0);
        }
        if !self.cache_covers(self.pos) {
            self.fill_cache()?;
            if self.cache_len == 0 {
                return Ok(0);
            }
        }
        let in_cache = (self.pos - self.cache_start) as usize;
        let avail = self.cache_len - in_cache;
        let want_bytes_left = (self.length - self.pos) as usize;
        let take = buf.len().min(avail).min(want_bytes_left);
        buf[..take].copy_from_slice(&self.cache[in_cache..in_cache + take]);
        self.pos += take as u64;
        Ok(take)
    }
}

impl Seek for PhysicalDisk {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::Current(o) => self.pos as i64 + o,
            SeekFrom::End(o) => self.length as i64 + o,
        };
        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of disk",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

impl BlockSource for PhysicalDisk {
    fn len_bytes(&self) -> Option<u64> {
        Some(self.length)
    }
}

fn open_handle(drive_number: u32) -> io::Result<HANDLE> {
    let path: Vec<u16> = format!("\\\\.\\PhysicalDrive{}", drive_number)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(|e: windows::core::Error| io::Error::other(e.message()))
    }
}

fn query_length(handle: HANDLE) -> io::Result<u64> {
    let mut info: MaybeUninit<GET_LENGTH_INFORMATION> = MaybeUninit::uninit();
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some(info.as_mut_ptr() as *mut c_void),
            std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
    };
    if let Err(e) = ok {
        return Err(io::Error::other(e.message()));
    }
    let info = unsafe { info.assume_init() };
    Ok(info.Length as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_returns_something_or_empty() {
        // Without admin, this returns []. We just check it doesn't panic.
        let _ = enumerate();
    }
}
