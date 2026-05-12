//! Recursive copy from a `MacFilesystem` to a local directory.
//!
//! Drives a robocopy-style "pull" of a subtree off an HFS+ (or, later,
//! APFS) volume to a destination directory on the local filesystem.
//! The intent is bulk data recovery — pull /Users to an external
//! backup drive, for example.
//!
//! ## Resumability
//!
//! Pull is restartable. For each file the worker checks the destination:
//!
//! - If the destination doesn't exist → copy.
//! - If it exists with matching size and mtime → skip.
//! - Otherwise → re-copy (overwrite).
//!
//! During the copy, bytes go to `<name>.applesauce-partial`. Only on
//! a clean finish does the partial get renamed to the final name. So
//! an interrupted run leaves a `.applesauce-partial` orphan and the
//! final-named file is still whatever the previous run produced (or
//! nothing). The next run treats the partial as garbage and starts
//! the file from scratch — atomicity per file, no half-written files
//! masquerading as complete.
//!
//! ## Filenames
//!
//! HFS+ allows characters Windows forbids (`:`, `<>|*?"`, NUL, etc.).
//! Each illegal character is replaced with `_`. Trailing dots/spaces
//! are stripped (Windows would do so silently). DOS-reserved stems
//! (`CON`, `LPT1`, …) get a leading `_`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::MacFilesystem;

/// Tunables for a pull run.
pub struct PullOptions {
    /// Per-read buffer. 1 MiB is a good default for sequential reads
    /// off a USB HFS+ disk.
    pub buffer_size: usize,
    /// Maximum allowed difference between source and destination mtime
    /// for a "skip, already copied" decision. FAT-like local
    /// filesystems round mtimes to 2-second precision, so a 2s window
    /// avoids false re-copies.
    pub mtime_tolerance_secs: u64,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            buffer_size: 1024 * 1024,
            mtime_tolerance_secs: 2,
        }
    }
}

/// Per-event callback payload. Callers translate these to log lines /
/// progress bars / status text.
#[derive(Debug, Clone)]
pub enum PullEvent {
    /// We're starting work in a directory (after creating it).
    EnteredDir { src: String, dst: PathBuf },
    /// File copy started. `size` is the logical byte size we'll write.
    StartedFile {
        src: String,
        dst: PathBuf,
        size: u64,
    },
    /// File copy finished (file is at its final name now).
    FinishedFile {
        src: String,
        dst: PathBuf,
        bytes_written: u64,
    },
    /// Destination existed with matching size + mtime; skipped.
    SkippedExisting {
        src: String,
        dst: PathBuf,
        size: u64,
    },
    /// One of HFS+'s internal "private data" directories was skipped.
    SkippedPrivate { src: String, name: String },
    /// Recoverable per-entry error. Pull continues with the next entry.
    Error { src: String, message: String },
}

/// Totals reported when pull finishes.
#[derive(Debug, Default, Clone, Copy)]
pub struct PullStats {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub skipped: u64,
    pub errors: u64,
}

/// Walk `src` on `fs`, mirroring the tree into `dst_dir`.
///
/// `dst_dir` is treated as a *container*. If `src` is `/Users`, files
/// land under `<dst_dir>/Users/...`; if `src` is a single file, it
/// lands at `<dst_dir>/<sanitized-name>`.
///
/// `cancel` is checked between files and between buffer reads. Set it
/// to abort cleanly; in-flight partials stay on disk as
/// `*.applesauce-partial` so the next run knows the work was aborted.
pub fn pull_tree<FS, F>(
    fs: &mut FS,
    src: &str,
    dst_dir: &Path,
    options: &PullOptions,
    cancel: &AtomicBool,
    on_event: &mut F,
) -> anyhow::Result<PullStats>
where
    FS: MacFilesystem + ?Sized,
    F: FnMut(PullEvent),
{
    std::fs::create_dir_all(dst_dir)?;

    let stat = fs.stat(src)?;
    let leaf = src
        .rsplit('/')
        .find(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| fs.volume_label().unwrap_or("root").to_string());
    let safe_leaf = sanitize_name(&leaf);
    let dst_root = dst_dir.join(&safe_leaf);

    let mut stats = PullStats::default();
    let mut buf = vec![0u8; options.buffer_size];

    if stat.is_dir {
        std::fs::create_dir_all(&dst_root)?;
        stats.dirs += 1;
        on_event(PullEvent::EnteredDir {
            src: src.to_string(),
            dst: dst_root.clone(),
        });
        pull_dir(
            fs, src, &dst_root, options, cancel, &mut stats, &mut buf, on_event,
        );
    } else {
        copy_file(
            fs,
            src,
            stat.size_bytes,
            stat.modified,
            &dst_root,
            options,
            cancel,
            &mut stats,
            &mut buf,
            on_event,
        );
    }

    Ok(stats)
}

#[allow(clippy::too_many_arguments)]
fn pull_dir<FS, F>(
    fs: &mut FS,
    src: &str,
    dst: &Path,
    options: &PullOptions,
    cancel: &AtomicBool,
    stats: &mut PullStats,
    buf: &mut [u8],
    on_event: &mut F,
) where
    FS: MacFilesystem + ?Sized,
    F: FnMut(PullEvent),
{
    if cancel.load(Ordering::Relaxed) {
        return;
    }

    let entries = match fs.list_dir(src) {
        Ok(v) => v,
        Err(e) => {
            stats.errors += 1;
            on_event(PullEvent::Error {
                src: src.to_string(),
                message: format!("list_dir: {e:#}"),
            });
            return;
        }
    };

    for e in entries {
        if cancel.load(Ordering::Relaxed) {
            return;
        }

        if is_private(&e.name) {
            stats.skipped += 1;
            on_event(PullEvent::SkippedPrivate {
                src: src.to_string(),
                name: e.name.clone(),
            });
            continue;
        }

        let safe = sanitize_name(&e.name);
        let dst_path = dst.join(&safe);
        let src_path = if src.ends_with('/') {
            format!("{src}{}", e.name)
        } else {
            format!("{src}/{}", e.name)
        };

        if e.is_dir {
            if let Err(err) = std::fs::create_dir_all(&dst_path) {
                stats.errors += 1;
                on_event(PullEvent::Error {
                    src: src_path.clone(),
                    message: format!("mkdir {}: {err}", dst_path.display()),
                });
                continue;
            }
            stats.dirs += 1;
            on_event(PullEvent::EnteredDir {
                src: src_path.clone(),
                dst: dst_path.clone(),
            });
            pull_dir(
                fs, &src_path, &dst_path, options, cancel, stats, buf, on_event,
            );
        } else {
            copy_file(
                fs,
                &src_path,
                e.size_bytes,
                e.modified,
                &dst_path,
                options,
                cancel,
                stats,
                buf,
                on_event,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_file<FS, F>(
    fs: &mut FS,
    src: &str,
    size: u64,
    src_modified: Option<SystemTime>,
    dst: &Path,
    options: &PullOptions,
    cancel: &AtomicBool,
    stats: &mut PullStats,
    buf: &mut [u8],
    on_event: &mut F,
) where
    FS: MacFilesystem + ?Sized,
    F: FnMut(PullEvent),
{
    // Already-copied? Skip if the existing file matches size + mtime.
    if already_matches(dst, size, src_modified, options.mtime_tolerance_secs) {
        stats.skipped += 1;
        on_event(PullEvent::SkippedExisting {
            src: src.to_string(),
            dst: dst.to_path_buf(),
            size,
        });
        return;
    }

    let partial = dst.with_extension(partial_extension(dst));
    let mut out = match std::fs::File::create(&partial) {
        Ok(f) => f,
        Err(e) => {
            stats.errors += 1;
            on_event(PullEvent::Error {
                src: src.to_string(),
                message: format!("create {}: {e}", partial.display()),
            });
            return;
        }
    };

    on_event(PullEvent::StartedFile {
        src: src.to_string(),
        dst: dst.to_path_buf(),
        size,
    });

    use std::io::Write;
    let mut offset = 0u64;
    while offset < size {
        if cancel.load(Ordering::Relaxed) {
            // Leave the partial in place so the next run is obviously
            // restartable. Don't count this as a finished file.
            return;
        }
        match fs.read_file_range(src, offset, buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = out.write_all(&buf[..n]) {
                    stats.errors += 1;
                    on_event(PullEvent::Error {
                        src: src.to_string(),
                        message: format!("write {}: {e}", partial.display()),
                    });
                    return;
                }
                offset += n as u64;
            }
            Err(e) => {
                stats.errors += 1;
                on_event(PullEvent::Error {
                    src: src.to_string(),
                    message: format!("read: {e:#}"),
                });
                return;
            }
        }
    }

    if let Err(e) = out.flush() {
        stats.errors += 1;
        on_event(PullEvent::Error {
            src: src.to_string(),
            message: format!("flush {}: {e}", partial.display()),
        });
        return;
    }
    drop(out);

    // Stamp the destination's mtime to match the source so future
    // restarts can use mtime-based skip detection.
    if let Some(t) = src_modified {
        let _ = set_mtime(&partial, t);
    }

    // Atomically promote partial → final. std::fs::rename on Windows
    // fails if the target exists; remove first.
    if dst.exists() {
        if let Err(e) = std::fs::remove_file(dst) {
            stats.errors += 1;
            on_event(PullEvent::Error {
                src: src.to_string(),
                message: format!("remove existing {}: {e}", dst.display()),
            });
            return;
        }
    }
    if let Err(e) = std::fs::rename(&partial, dst) {
        stats.errors += 1;
        on_event(PullEvent::Error {
            src: src.to_string(),
            message: format!("rename {} -> {}: {e}", partial.display(), dst.display()),
        });
        return;
    }

    stats.files += 1;
    stats.bytes += offset;
    on_event(PullEvent::FinishedFile {
        src: src.to_string(),
        dst: dst.to_path_buf(),
        bytes_written: offset,
    });
}

fn partial_extension(dst: &Path) -> String {
    match dst.extension().and_then(|s| s.to_str()) {
        Some(ext) => format!("{ext}.applesauce-partial"),
        None => "applesauce-partial".to_string(),
    }
}

fn already_matches(
    dst: &Path,
    src_size: u64,
    src_mtime: Option<SystemTime>,
    tolerance_secs: u64,
) -> bool {
    let Ok(md) = std::fs::metadata(dst) else {
        return false;
    };
    if md.len() != src_size {
        return false;
    }
    let Some(src_t) = src_mtime else {
        return false;
    };
    let Ok(dst_t) = md.modified() else {
        return false;
    };
    let diff = match src_t.duration_since(dst_t) {
        Ok(d) => d.as_secs(),
        Err(e) => e.duration().as_secs(),
    };
    diff <= tolerance_secs
}

#[cfg(windows)]
fn set_mtime(path: &Path, mtime: SystemTime) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(0x02000000) // FILE_FLAG_BACKUP_SEMANTICS
        .open(path)?;
    file.set_modified(mtime)?;
    Ok(())
}

#[cfg(not(windows))]
fn set_mtime(path: &Path, mtime: SystemTime) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_modified(mtime)?;
    Ok(())
}

/// Replace Windows-illegal filename characters with `_`. Returns at
/// least one character; empty / dot-only names become `_`.
pub fn sanitize_name(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        let safe = match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        };
        escaped.push(safe);
    }

    let trimmed = escaped.trim_end_matches(['.', ' ']).to_string();
    if trimmed.is_empty() {
        return "_".to_string();
    }

    let body = if trimmed.len() < escaped.len() {
        format!("{trimmed}_")
    } else {
        trimmed
    };

    let stem = body.split('.').next().unwrap_or(&body).to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        format!("_{body}")
    } else {
        body
    }
}

/// HFS+ keeps internal bookkeeping in two specially-named folders at
/// the volume root. Copying them would balloon the destination with
/// hard-link inode shadows.
pub fn is_private(name: &str) -> bool {
    matches!(
        name,
        ".HFS+ Private Directory Data\u{0d}"
            | "\u{0}\u{0}\u{0}\u{0}HFS+ Private Data"
            | ".HFS+ Private Directory Data"
            | "    HFS+ Private Data"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(sanitize_name("hello"), "hello");
        assert_eq!(sanitize_name("a:b"), "a_b");
        assert_eq!(sanitize_name(r"a/b\c"), "a_b_c");
        assert_eq!(sanitize_name("foo*"), "foo_");
        assert_eq!(sanitize_name("good?"), "good_");
    }

    #[test]
    fn sanitize_handles_trailing_dot_space() {
        let s = sanitize_name("foo.");
        assert!(s.ends_with('_'));
        let s = sanitize_name("bar ");
        assert!(s.ends_with('_'));
    }

    #[test]
    fn sanitize_escapes_reserved_dos_names() {
        assert_eq!(sanitize_name("CON"), "_CON");
        assert_eq!(sanitize_name("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_name("LPT3"), "_LPT3");
        assert_eq!(sanitize_name("Aux"), "_Aux");
    }

    #[test]
    fn sanitize_empty_or_dots() {
        assert_eq!(sanitize_name(""), "_");
        assert_eq!(sanitize_name("."), "_");
        assert_eq!(sanitize_name(".."), "_");
    }

    #[test]
    fn partial_extension_appends() {
        assert_eq!(
            partial_extension(Path::new("foo.txt")),
            "txt.applesauce-partial"
        );
        assert_eq!(partial_extension(Path::new("foo")), "applesauce-partial");
    }
}
