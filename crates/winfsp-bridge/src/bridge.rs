//! Read-only adapter from `fs_core::MacFilesystem` to WinFsp.
//!
//! WinFsp drives the filesystem from multiple worker threads. Our
//! `MacFilesystem` impls share a single `Read + Seek` source and need
//! exclusive access for every catalog descent / fork read, so the
//! bridge serializes everything behind a `Mutex`. This is fine for
//! browse / copy workloads on a single physical disk; concurrent
//! random reads from multiple processes will queue.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Context as _;
use fs_core::{MacFilesystem, Stat};
use windows::Win32::Foundation::{
    STATUS_END_OF_FILE, STATUS_FILE_IS_A_DIRECTORY, STATUS_IO_DEVICE_ERROR, STATUS_NOT_A_DIRECTORY,
    STATUS_OBJECT_NAME_NOT_FOUND,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY};
use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::{FspError, U16CStr};

/// 100ns intervals between Windows FILETIME epoch (1601-01-01) and the
/// Unix epoch (1970-01-01).
const FILETIME_UNIX_EPOCH: u64 = 11_644_473_600 * 10_000_000;

/// Convert a `SystemTime` to a Windows FILETIME (100ns ticks since
/// 1601-01-01 UTC). Returns 0 for times before the Unix epoch.
fn systime_to_filetime(t: SystemTime) -> u64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => {
            let ticks = d.as_secs() * 10_000_000 + u64::from(d.subsec_nanos()) / 100;
            FILETIME_UNIX_EPOCH + ticks
        }
        Err(_) => 0,
    }
}

/// Translate a WinFsp path (`\Users\dhh`, UTF-16, backslashes) to a
/// POSIX-style path our `MacFilesystem` expects (`/Users/dhh`). An
/// empty WinFsp path becomes `/`.
fn winfsp_path_to_posix(p: &U16CStr) -> String {
    let s = p.to_string_lossy();
    let mut out = String::with_capacity(s.len().max(1));
    for ch in s.chars() {
        out.push(if ch == '\\' { '/' } else { ch });
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

fn file_attributes_for(stat: &Stat) -> u32 {
    let mut a = FILE_ATTRIBUTE_READONLY.0;
    if stat.is_dir {
        a |= FILE_ATTRIBUTE_DIRECTORY.0;
    }
    a
}

fn fill_file_info(info: &mut FileInfo, stat: &Stat) {
    info.file_attributes = file_attributes_for(stat);
    info.reparse_tag = 0;
    info.allocation_size = (stat.size_bytes + 4095) & !4095;
    info.file_size = stat.size_bytes;
    let modified = stat.modified.map(systime_to_filetime).unwrap_or(0);
    let created = stat.created.map(systime_to_filetime).unwrap_or(modified);
    info.creation_time = created;
    info.last_access_time = modified;
    info.last_write_time = modified;
    info.change_time = modified;
    info.index_number = 0;
    info.hard_links = 0;
    info.ea_size = 0;
}

/// Cached metadata for an open file/directory handle.
pub struct Handle {
    path: String,
    is_dir: bool,
    size: u64,
}

/// Bridge between a `MacFilesystem` reader and WinFsp.
pub struct Bridge {
    fs: Arc<Mutex<dyn MacFilesystem + Send>>,
    total_bytes: u64,
    label: String,
}

impl Bridge {
    /// `total_bytes` is the size of the underlying volume (used for
    /// `get_volume_info`). `label` is the volume name to show in
    /// Explorer.
    pub fn new<FS>(fs: FS, total_bytes: u64) -> Self
    where
        FS: MacFilesystem + Send + 'static,
    {
        let label = fs.volume_label().unwrap_or_default().to_string();
        Self {
            fs: Arc::new(Mutex::new(fs)),
            total_bytes,
            label,
        }
    }

    fn stat(&self, path: &str) -> winfsp::Result<Stat> {
        let mut fs = self.fs.lock().expect("fs mutex poisoned");
        fs.stat(path)
            .map_err(|_| FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND.0))
    }
}

impl FileSystemContext for Bridge {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = winfsp_path_to_posix(file_name);
        let stat = self.stat(&path)?;
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: file_attributes_for(&stat),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = winfsp_path_to_posix(file_name);
        let stat = self.stat(&path)?;
        fill_file_info(file_info.as_mut(), &stat);
        Ok(Handle {
            path,
            is_dir: stat.is_dir,
            size: stat.size_bytes,
        })
    }

    fn close(&self, _context: Self::FileContext) {}

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        if context.is_dir {
            return Err(FspError::NTSTATUS(STATUS_FILE_IS_A_DIRECTORY.0));
        }
        if offset >= context.size {
            return Err(FspError::NTSTATUS(STATUS_END_OF_FILE.0));
        }
        let want = std::cmp::min(buffer.len() as u64, context.size - offset) as usize;
        let mut fs = self.fs.lock().expect("fs mutex poisoned");
        let n = fs
            .read_file_range(&context.path, offset, &mut buffer[..want])
            .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR.0))?;
        Ok(n as u32)
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let stat = self.stat(&context.path)?;
        fill_file_info(file_info, &stat);
        Ok(())
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> winfsp::Result<()> {
        out.total_size = self.total_bytes;
        out.free_size = 0;
        out.set_volume_label(&self.label);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker<'_>,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if !context.is_dir {
            return Err(FspError::NTSTATUS(STATUS_NOT_A_DIRECTORY.0));
        }
        let entries = {
            let mut fs = self.fs.lock().expect("fs mutex poisoned");
            fs.list_dir(&context.path)
                .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR.0))?
        };

        // WinFsp pages directory results: `marker`, if present, names
        // the last entry the kernel saw, and we must emit entries that
        // sort strictly after it. `marker.inner()` is a raw UTF-16 slice
        // (no NUL terminator).
        let marker_str: Option<String> = marker.inner().map(String::from_utf16_lossy);
        let mut started = marker_str.is_none();
        let mut cursor: u32 = 0;

        for entry in entries {
            if !started {
                if Some(entry.name.as_str()) == marker_str.as_deref() {
                    started = true;
                }
                continue;
            }
            let mut di: DirInfo<255> = DirInfo::new();
            if di.set_name(entry.name.as_str()).is_err() {
                continue; // name too long; skip
            }
            fill_file_info(di.file_info_mut(), &entry);
            if !di.append_to_buffer(buffer, &mut cursor) {
                break;
            }
        }

        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }
}

/// A live, mounted filesystem. Drop or call [`MountedHost::unmount`]
/// to tear it down.
pub struct MountedHost {
    host: FileSystemHost<Bridge>,
}

impl MountedHost {
    /// Unmount and stop the filesystem. Idempotent.
    pub fn unmount(mut self) {
        self.host.stop();
        self.host.unmount();
    }
}

impl Drop for MountedHost {
    fn drop(&mut self) {
        self.host.stop();
        self.host.unmount();
    }
}

/// Mount `fs` at `mountpoint` (e.g. `"Z:"` for a drive letter, or a
/// directory path for a mountpoint). `total_bytes` should be the size
/// of the underlying volume.
pub fn mount<FS>(fs: FS, total_bytes: u64, mountpoint: &str) -> anyhow::Result<MountedHost>
where
    FS: MacFilesystem + Send + 'static,
{
    let _init = winfsp::winfsp_init().context("winfsp_init failed")?;

    let mut params = VolumeParams::new();
    params
        .filesystem_name("HFS+")
        .read_only_volume(true)
        .case_preserved_names(true)
        .case_sensitive_search(false)
        .unicode_on_disk(true)
        .sector_size(512)
        .sectors_per_allocation_unit(8)
        .max_component_length(255)
        .file_info_timeout(1000);

    let bridge = Bridge::new(fs, total_bytes);
    let mut host = FileSystemHost::new(params, bridge).context("FileSystemHost::new failed")?;
    host.mount(mountpoint)
        .with_context(|| format!("mount {mountpoint} failed"))?;
    host.start().context("FileSystemHost::start failed")?;
    Ok(MountedHost { host })
}
