//! applesauce-cat — exercise the HFS+ reader against a disk image or
//! a physical Mac drive plugged into Windows.
//!
//! Usage:
//!   applesauce-cat <image>                          — volume info
//!   applesauce-cat <image> ls [path]                — list a directory
//!   applesauce-cat <image> cat <path>               — dump a file
//!   applesauce-cat <image> pull <src> <dst-dir>     — recursive copy off the volume
//!
//!   applesauce-cat --disk N                         — volume info
//!   applesauce-cat --disk N ls [path]               — list a directory
//!   applesauce-cat --disk N cat <path>              — dump a file
//!   applesauce-cat --disk N pull <src> <dst-dir>    — recursive copy off the disk
//!
//! If the source contains a partition table, the first Mac-typed
//! volume is auto-selected. If there's no partition table, the source
//! is treated as a bare HFS+ volume.

use std::env;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use block_source::image::ImageFile;
use block_source::partition::{self, Partition, PartitionScheme};
use block_source::window::Window;
use block_source::BlockSource;
use fs_core::hfsplus::Hfsplus;
use fs_core::MacFilesystem;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("applesauce-cat: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }

    // Source selection: --disk N | <image-path>
    if args[0] == "--disk" {
        #[cfg(windows)]
        {
            let n: u32 = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("--disk requires a drive number"))?
                .parse()?;
            let command = args.get(2).map(|s| s.as_str()).unwrap_or("info");
            let rest: Vec<&str> = args.iter().skip(3).map(|s| s.as_str()).collect();
            let source = block_source::physical::PhysicalDisk::open(n)?;
            let (volume_offset, mut fs) = open_first_hfsplus(source)?;
            return dispatch(command, &rest, &mut fs, volume_offset);
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("--disk is Windows-only");
        }
    }

    let image_path = &args[0];
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("info");
    let rest: Vec<&str> = args.iter().skip(2).map(|s| s.as_str()).collect();

    let source = ImageFile::open(image_path)?;
    let (volume_offset, mut fs) = open_first_hfsplus(source)?;
    dispatch(command, &rest, &mut fs, volume_offset)
}

fn dispatch<S: BlockSource + 'static>(
    command: &str,
    rest: &[&str],
    fs: &mut Hfsplus<Window<S>>,
    volume_offset: u64,
) -> anyhow::Result<()> {
    match command {
        "info" => cmd_info(fs, volume_offset),
        "ls" => cmd_ls(fs, rest.first().copied().unwrap_or("/")),
        "cat" => {
            let path = rest
                .first()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("cat needs a path"))?;
            cmd_cat(fs, path)
        }
        "pull" => {
            let src = rest
                .first()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("pull needs a source path"))?;
            let dst = rest
                .get(1)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("pull needs a destination directory"))?;
            cmd_pull(fs, src, Path::new(dst))
        }
        other => anyhow::bail!("unknown subcommand {other:?}"),
    }
}

fn print_usage() {
    eprintln!(
        "applesauce-cat {}\n\
         \n\
         USAGE:\n  \
           applesauce-cat <image>                       # volume info\n  \
           applesauce-cat <image> ls [path]             # list directory (default: /)\n  \
           applesauce-cat <image> cat <path>            # dump file to stdout\n  \
           applesauce-cat <image> pull <src> <dst-dir>  # recursive copy off the volume\n  \
           applesauce-cat --disk N [...]                # read \\\\.\\PhysicalDriveN (Admin)\n\
         \n\
         pull copies a single file or a whole directory subtree from the\n\
         HFS+ volume to <dst-dir> on the local filesystem. Filenames with\n\
         characters illegal on Windows (`:`, `\\`, `<>|*?`, etc.) get those\n\
         characters replaced with `_`.",
        env!("CARGO_PKG_VERSION"),
    );
}

/// Open `source`, find the first Mac-typed partition (or treat the
/// whole source as a volume if there's no partition table), and hand
/// back a windowed `Hfsplus`. The reported `volume_offset` is the byte
/// offset of the chosen volume within `source`.
fn open_first_hfsplus<S: BlockSource + 'static>(
    mut source: S,
) -> anyhow::Result<(u64, Hfsplus<Window<S>>)> {
    let parts = partition::probe(&mut source)?;

    let chosen: Option<Partition> = parts
        .iter()
        .find(|p| {
            matches!(p.scheme, PartitionScheme::Gpt | PartitionScheme::Apm) && p.is_mac_filesystem()
        })
        .cloned();

    let (start, length) = match chosen {
        Some(p) => (p.start_byte, p.length_bytes),
        None => {
            let len = source.len_bytes().ok_or_else(|| {
                anyhow::anyhow!("source has no known length and no partition table")
            })?;
            (0, len)
        }
    };

    let window = Window::new(source, start, length)?;
    let fs = Hfsplus::open(window, 0)?;
    Ok((start, fs))
}

fn cmd_info<S: BlockSource>(fs: &Hfsplus<Window<S>>, volume_offset: u64) -> anyhow::Result<()> {
    println!("volume offset: {volume_offset} bytes");
    match fs.volume_label() {
        Some(label) => println!("volume label:  {label}"),
        None => println!("volume label:  (unknown)"),
    }
    Ok(())
}

fn cmd_ls<S: std::io::Read + std::io::Seek + Send>(
    fs: &mut Hfsplus<S>,
    path: &str,
) -> anyhow::Result<()> {
    let entries = fs.list_dir(path)?;
    if entries.is_empty() {
        println!("(empty)");
        return Ok(());
    }
    println!("{:<8}  {:>14}  NAME", "KIND", "SIZE");
    for e in entries {
        println!(
            "{:<8}  {:>14}  {}",
            if e.is_dir { "DIR" } else { "FILE" },
            e.size_bytes,
            e.name,
        );
    }
    Ok(())
}

fn cmd_cat<S: std::io::Read + std::io::Seek + Send>(
    fs: &mut Hfsplus<S>,
    path: &str,
) -> anyhow::Result<()> {
    let st = fs.stat(path)?;
    if st.is_dir {
        anyhow::bail!("{path} is a directory");
    }
    let mut buf = vec![0u8; 64 * 1024];
    let mut offset = 0u64;
    let mut stdout = std::io::stdout().lock();
    while offset < st.size_bytes {
        let n = fs.read_file_range(path, offset, &mut buf)?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n])?;
        offset += n as u64;
    }
    stdout.flush()?;
    Ok(())
}

#[derive(Default)]
struct PullStats {
    files: u64,
    dirs: u64,
    bytes: u64,
    skipped: u64,
    errors: u64,
}

fn cmd_pull<S: std::io::Read + std::io::Seek + Send>(
    fs: &mut Hfsplus<S>,
    src: &str,
    dst_dir: &Path,
) -> anyhow::Result<()> {
    let st = fs.stat(src)?;

    // dst_dir is treated as the *container*. If src is a directory
    // named "Users", we copy into "<dst_dir>/Users/...". If src is a
    // single file, we copy to "<dst_dir>/<filename>".
    std::fs::create_dir_all(dst_dir)?;

    let start = Instant::now();
    let mut stats = PullStats::default();
    let mut buf = vec![0u8; 1024 * 1024];

    let leaf_name = src
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let safe_leaf = if leaf_name.is_empty() {
        fs.volume_label().unwrap_or("root").to_string()
    } else {
        sanitize_name(&leaf_name)
    };
    let dst_root = dst_dir.join(&safe_leaf);

    if st.is_dir {
        std::fs::create_dir_all(&dst_root)?;
        stats.dirs += 1;
        pull_dir(fs, src, &dst_root, &mut stats, &mut buf)?;
    } else {
        copy_file(fs, src, st.size_bytes, &dst_root, &mut stats, &mut buf)?;
    }

    let elapsed = start.elapsed();
    let mb = stats.bytes as f64 / (1u64 << 20) as f64;
    let mbps = if elapsed.as_secs_f64() > 0.0 {
        mb / elapsed.as_secs_f64()
    } else {
        0.0
    };
    eprintln!(
        "\npull complete: {} files / {} dirs / {:.2} MiB in {:.1}s ({:.1} MiB/s), \
         {} skipped, {} errors",
        stats.files,
        stats.dirs,
        mb,
        elapsed.as_secs_f64(),
        mbps,
        stats.skipped,
        stats.errors
    );
    Ok(())
}

fn pull_dir<S: std::io::Read + std::io::Seek + Send>(
    fs: &mut Hfsplus<S>,
    src: &str,
    dst: &Path,
    stats: &mut PullStats,
    buf: &mut [u8],
) -> anyhow::Result<()> {
    let entries = match fs.list_dir(src) {
        Ok(v) => v,
        Err(e) => {
            stats.errors += 1;
            eprintln!("  ! list_dir {src}: {e:#}");
            return Ok(());
        }
    };

    for e in entries {
        // Skip the catalog's private-data directories — they hold
        // implementation detail (hard-link inodes, etc.) and would
        // create huge, mostly-useless folders on the destination.
        if is_private(&e.name) {
            stats.skipped += 1;
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
                eprintln!("  ! mkdir {}: {err}", dst_path.display());
                continue;
            }
            stats.dirs += 1;
            pull_dir(fs, &src_path, &dst_path, stats, buf)?;
        } else {
            copy_file(fs, &src_path, e.size_bytes, &dst_path, stats, buf)?;
        }
    }
    Ok(())
}

fn copy_file<S: std::io::Read + std::io::Seek + Send>(
    fs: &mut Hfsplus<S>,
    src: &str,
    size: u64,
    dst: &Path,
    stats: &mut PullStats,
    buf: &mut [u8],
) -> anyhow::Result<()> {
    let mut out = match std::fs::File::create(dst) {
        Ok(f) => f,
        Err(e) => {
            stats.errors += 1;
            eprintln!("  ! create {}: {e}", dst.display());
            return Ok(());
        }
    };

    let mut offset = 0u64;
    while offset < size {
        match fs.read_file_range(src, offset, buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = out.write_all(&buf[..n]) {
                    stats.errors += 1;
                    eprintln!("  ! write {}: {e}", dst.display());
                    return Ok(());
                }
                offset += n as u64;
            }
            Err(e) => {
                stats.errors += 1;
                eprintln!("  ! read {src}: {e:#}");
                return Ok(());
            }
        }
    }

    if let Err(e) = out.flush() {
        stats.errors += 1;
        eprintln!("  ! flush {}: {e}", dst.display());
        return Ok(());
    }

    stats.files += 1;
    stats.bytes += offset;
    if stats.files.is_multiple_of(50) {
        eprint!(
            "  {} files, {:.2} MiB\r",
            stats.files,
            stats.bytes as f64 / (1u64 << 20) as f64
        );
        let _ = std::io::stderr().flush();
    }
    Ok(())
}

/// Replace Windows-illegal filename characters with `_`. Returns at
/// least one character; empty / dot-only names become `_`.
fn sanitize_name(name: &str) -> String {
    // 1. Replace Windows-illegal chars + control chars with `_`.
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        let safe = match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        };
        escaped.push(safe);
    }

    // 2. Windows silently strips trailing dots and spaces. Trim them
    //    explicitly so the file lands where we expect.
    let trimmed = escaped.trim_end_matches(['.', ' ']).to_string();

    // 3. Empty or only-dots (".", "..") gets a single underscore.
    if trimmed.is_empty() {
        return "_".to_string();
    }

    // 4. If we stripped any trailing dot/space, signal that with a
    //    suffix so the result is distinguishable from the original.
    let body = if trimmed.len() < escaped.len() {
        format!("{trimmed}_")
    } else {
        trimmed
    };

    // 5. Reserved DOS device names get a leading underscore so they
    //    don't clash with the Windows namespace.
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
/// hard-link inode shadows, and the names contain non-printable
/// characters that confuse shells.
fn is_private(name: &str) -> bool {
    matches!(
        name,
        ".HFS+ Private Directory Data\u{0d}"
            | "\u{0}\u{0}\u{0}\u{0}HFS+ Private Data"
            // Also skip the catalog's display variants we surface as plain text.
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
        // Windows strips these silently and the file would land
        // somewhere unexpected; replace with `_` to make it explicit.
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
        // case-insensitive
        assert_eq!(sanitize_name("Aux"), "_Aux");
    }

    #[test]
    fn sanitize_empty_or_dots() {
        assert_eq!(sanitize_name(""), "_");
        assert_eq!(sanitize_name("."), "_");
        assert_eq!(sanitize_name(".."), "_");
    }
}
