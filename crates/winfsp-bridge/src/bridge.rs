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
    STATUS_END_OF_FILE, STATUS_FILE_IS_A_DIRECTORY, STATUS_INTERNAL_ERROR, STATUS_IO_DEVICE_ERROR,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_NOT_FOUND,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY};
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
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
///
/// `dir_buffer` is winfsp's per-handle paged directory cache: on the
/// first `read_directory` call for a directory (`marker.is_none()`)
/// we acquire and populate it with the full sorted listing; every
/// subsequent paged call serves out of the buffer with no further
/// catalog reads. For file handles the buffer is constructed but
/// never used — `DirBuffer::new` is cheap (an empty handle).
pub struct Handle {
    path: String,
    is_dir: bool,
    size: u64,
    dir_buffer: DirBuffer,
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
        let mut fs = self
            .fs
            .lock()
            .map_err(|_| FspError::NTSTATUS(STATUS_INTERNAL_ERROR.0))?;
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
            dir_buffer: DirBuffer::new(),
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
        let mut fs = self
            .fs
            .lock()
            .map_err(|_| FspError::NTSTATUS(STATUS_INTERNAL_ERROR.0))?;
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

        // First call from the kernel for this open handle (no marker):
        // list the directory once and drop everything into the per-
        // handle DirBuffer. Subsequent paged calls hit DirBuffer::read
        // with the kernel-supplied marker, and DirBuffer's internal
        // sorted-by-name storage serves the next page without us
        // walking the catalog again. Without this we re-list 500+
        // entries on every page, which is what was making Explorer
        // hang on big directories.
        if marker.is_none() {
            let entries = {
                let mut fs = self
                    .fs
                    .lock()
                    .map_err(|_| FspError::NTSTATUS(STATUS_INTERNAL_ERROR.0))?;
                fs.list_dir(&context.path)
                    .map_err(|_| FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR.0))?
            };

            let hint = entries.len().min(u32::MAX as usize) as u32;
            if let Ok(lock) = context.dir_buffer.acquire(true, Some(hint)) {
                for entry in &entries {
                    let mut di: DirInfo<255> = DirInfo::new();
                    if di.set_name(entry.name.as_str()).is_err() {
                        continue; // name too long, skip
                    }
                    fill_file_info(di.file_info_mut(), entry);
                    if lock.write(&mut di).is_err() {
                        break; // buffer full
                    }
                }
            }
        }

        Ok(context.dir_buffer.read(marker, buffer))
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
    // WinFsp 2.x ships in a Side-by-Side install directory. Without
    // WinFsp's bin dir on PATH, the delay-loaded winfsp-x64.dll fails
    // to resolve. The install dir lives in the registry; prepend it
    // to PATH ourselves before the first call into the WinFsp crate.
    prepend_winfsp_to_path();

    let _init = winfsp::winfsp_init().map_err(|e| match e {
        FspError::WIN32(c) => anyhow::anyhow!("winfsp_init failed: WIN32 error 0x{c:08X} ({c})"),
        FspError::NTSTATUS(c) => {
            anyhow::anyhow!("winfsp_init failed: NTSTATUS 0x{:08X}", c as u32)
        }
        other => anyhow::anyhow!("winfsp_init failed: {other:?}"),
    })?;

    let mut params = VolumeParams::new();
    params
        .filesystem_name("HFS+")
        .read_only_volume(true)
        .case_preserved_names(true)
        .case_sensitive_search(false)
        .unicode_on_disk(true)
        // Windows expects 4096-byte sectors for typical I/O paths. The
        // WinFsp passthrough / ntptfs samples both use 4096; 512 makes
        // the kernel issue many more, smaller IRPs.
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        // ---- Cache timeouts (all milliseconds) ----
        //
        // Read-only volume: file metadata, security descriptors, and
        // streams never change once a volume is mounted, so the kernel
        // can hold answers for a long time. 30 s for metadata + dir
        // listings means Explorer doesn't re-poll us while the user is
        // clicking around; 10 min for security / EAs / streams covers
        // a full browse session.
        //
        // We deliberately avoid `u32::MAX` because that flips WinFsp
        // into "delegate to the Windows Cache Manager" mode, which
        // serialises overlapping reads of the same file.
        .file_info_timeout(30_000)
        .dir_info_timeout(30_000)
        .volume_info_timeout(60_000)
        .security_timeout(600_000)
        .stream_info_timeout(600_000)
        .extended_attribute_timeout(600_000)
        // ---- Read-only-specific perf flags ----
        // Without this WinFsp posts a Cleanup IRP on every file close,
        // even pure reads. Tutorial calls this out as a key perf win.
        .post_cleanup_when_modified_only(true)
        // Read-only volume never deletes — skip the disposition IRPs.
        .post_disposition_only_when_necessary(true)
        // Keep files on Windows' standby list after close so a repeat
        // open hits page cache.
        .flush_and_purge_on_cleanup(false)
        // We surface no ACLs (return a constant attribute set); skip
        // the per-file security descriptor round trip.
        .persistent_acls(false);

    let bridge = Bridge::new(fs, total_bytes);
    let mut host = FileSystemHost::new(params, bridge).context("FileSystemHost::new failed")?;
    host.mount(mountpoint)
        .with_context(|| format!("mount {mountpoint} failed"))?;
    host.start().context("FileSystemHost::start failed")?;
    Ok(MountedHost { host })
}

/// Look up WinFsp's install dir in the registry and prepend its `bin`
/// to PATH for this process. No-op if the registry key is missing —
/// `winfsp_init` will then surface a clearer error.
fn prepend_winfsp_to_path() {
    use std::env;

    // 32-bit view: SxsDir / InstallDir live under WOW6432Node on x64.
    let install = [
        "HKLM\\SOFTWARE\\WOW6432Node\\WinFsp",
        "HKLM\\SOFTWARE\\WinFsp",
    ]
    .iter()
    .find_map(|root| read_install_dir(root));

    let Some(install) = install else {
        return;
    };

    let bin = format!("{}\\bin", install.trim_end_matches('\\'));
    let prior = env::var_os("PATH").unwrap_or_default();
    let mut combined = std::ffi::OsString::from(&bin);
    combined.push(";");
    combined.push(&prior);
    env::set_var("PATH", combined);
}

fn read_install_dir(root: &str) -> Option<String> {
    use std::process::Command;
    let out = Command::new("reg")
        .args(["query", root, "/v", "InstallDir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("InstallDir") {
            // "InstallDir    REG_SZ    C:\Program Files (x86)\WinFsp\"
            let tail = rest.trim_start();
            // Skip the type token.
            let mut it = tail.splitn(2, char::is_whitespace);
            let _ty = it.next();
            if let Some(value) = it.next() {
                let v = value.trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}
