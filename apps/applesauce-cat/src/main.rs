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
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use block_source::image::ImageFile;
use block_source::partition::{self, Partition, PartitionScheme, APPLE_APFS_CONTAINER_GUID};
use block_source::window::Window;
use block_source::BlockSource;
use fs_core::apfs::{ApfsContainer, ApfsVolume};
use fs_core::hfsplus::Hfsplus;
use fs_core::pull::{pull_tree, PullEvent, PullOptions};
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
            if command == "apfs" {
                return cmd_apfs(move || Ok(block_source::physical::PhysicalDisk::open(n)?));
            }
            if command == "apfs-ls" {
                let vol = rest
                    .first()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("apfs-ls needs a volume name"))?;
                let path = rest.get(1).copied().unwrap_or("/");
                return cmd_apfs_ls(
                    move || Ok(block_source::physical::PhysicalDisk::open(n)?),
                    vol,
                    path,
                );
            }
            if command == "apfs-cat" {
                let vol = rest
                    .first()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("apfs-cat needs a volume name"))?;
                let path = rest
                    .get(1)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("apfs-cat needs a file path"))?;
                return cmd_apfs_cat(
                    move || Ok(block_source::physical::PhysicalDisk::open(n)?),
                    vol,
                    path,
                );
            }
            if command == "apfs-bench" {
                let vol = rest
                    .first()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("apfs-bench needs a volume name"))?;
                let path = rest.get(1).copied().unwrap_or("/");
                return cmd_apfs_bench(
                    move || Ok(block_source::physical::PhysicalDisk::open(n)?),
                    vol,
                    path,
                );
            }
            if command == "apfs-pull" {
                let skip_existing = rest.contains(&"--skip-existing");
                let pos: Vec<&str> = rest
                    .iter()
                    .copied()
                    .filter(|a| !a.starts_with("--"))
                    .collect();
                let vol = pos
                    .first()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("apfs-pull needs a volume name"))?;
                let src = pos
                    .get(1)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("apfs-pull needs a source path"))?;
                let dst = pos
                    .get(2)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("apfs-pull needs a destination dir"))?;
                return cmd_apfs_pull(
                    move || Ok(block_source::physical::PhysicalDisk::open(n)?),
                    vol,
                    src,
                    Path::new(dst),
                    skip_existing,
                );
            }
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

    if command == "apfs" {
        let path = image_path.clone();
        return cmd_apfs(move || Ok(ImageFile::open(&path)?));
    }
    if command == "apfs-ls" {
        let vol = rest
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("apfs-ls needs a volume name"))?
            .to_string();
        let path = rest.get(1).copied().unwrap_or("/").to_string();
        let img = image_path.clone();
        return cmd_apfs_ls(move || Ok(ImageFile::open(&img)?), &vol, &path);
    }
    if command == "apfs-cat" {
        let vol = rest
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("apfs-cat needs a volume name"))?
            .to_string();
        let path = rest
            .get(1)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("apfs-cat needs a file path"))?
            .to_string();
        let img = image_path.clone();
        return cmd_apfs_cat(move || Ok(ImageFile::open(&img)?), &vol, &path);
    }

    let source = ImageFile::open(image_path)?;
    let (volume_offset, mut fs) = open_first_hfsplus(source)?;
    dispatch(command, &rest, &mut fs, volume_offset)
}

/// True if a partition holds an APFS container (GPT GUID or APM label).
fn is_apfs_container(p: &Partition) -> bool {
    matches!(
        p.type_id.as_str(),
        APPLE_APFS_CONTAINER_GUID | "Apple_APFS" | "Apple_APFS_Container"
    )
}

/// Enumerate APFS containers and the volumes inside them. `open`
/// produces a fresh source each call so we can window into each
/// container independently (a disk may hold several).
fn cmd_apfs<S, F>(open: F) -> anyhow::Result<()>
where
    S: BlockSource + 'static,
    F: Fn() -> anyhow::Result<S>,
{
    let mut probe_src = open()?;
    let parts = partition::probe(&mut probe_src)?;
    drop(probe_src);

    // No partition table → treat the whole source as one container.
    if parts.is_empty() {
        let container = ApfsContainer::open(open()?)?;
        return print_container("(whole disk)", container);
    }

    let containers: Vec<&Partition> = parts.iter().filter(|p| is_apfs_container(p)).collect();
    if containers.is_empty() {
        println!(
            "No APFS containers found ({} partition(s) present).",
            parts.len()
        );
        return Ok(());
    }

    for p in containers {
        let window = Window::new(open()?, p.start_byte, p.length_bytes)?;
        match ApfsContainer::open(window) {
            Ok(container) => print_container(&p.name, container)?,
            Err(e) => eprintln!("container “{}”: {e:#}", p.name),
        }
    }
    Ok(())
}

/// List a directory inside a named APFS volume. Searches every APFS
/// container on the source for a volume whose name matches `vol_name`
/// (case-insensitive), opens it, and lists `path`.
/// Search every APFS container on the source for a volume named
/// `vol_name` (case-insensitive) and open it. `open` yields a fresh
/// source per call so each container gets an independent window.
fn find_apfs_volume<S, F>(open: F, vol_name: &str) -> anyhow::Result<ApfsVolume<Window<S>>>
where
    S: BlockSource + 'static,
    F: Fn() -> anyhow::Result<S>,
{
    let mut probe_src = open()?;
    let parts = partition::probe(&mut probe_src)?;
    drop(probe_src);

    // Each entry is the window into one container; `None` = whole source.
    let targets: Vec<Option<(u64, u64)>> = if parts.is_empty() {
        vec![None]
    } else {
        let cs: Vec<Option<(u64, u64)>> = parts
            .iter()
            .filter(|p| is_apfs_container(p))
            .map(|p| Some((p.start_byte, p.length_bytes)))
            .collect();
        if cs.is_empty() {
            anyhow::bail!("no APFS containers found on source");
        }
        cs
    };

    let mut available = Vec::new();
    for t in targets {
        // Always go through a Window so both arms have the same type.
        let (start, len) = match t {
            Some((s, l)) => (s, l),
            None => {
                let probe = open()?;
                let len = probe
                    .len_bytes()
                    .ok_or_else(|| anyhow::anyhow!("source length unknown"))?;
                (0, len)
            }
        };
        let mut container = ApfsContainer::open(Window::new(open()?, start, len)?)?;
        let vols = container.volumes()?;
        if let Some(info) = vols
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(vol_name))
            .cloned()
        {
            if info.encrypted {
                anyhow::bail!(
                    "volume “{}” is FileVault-encrypted; reading it needs the key (not yet supported)",
                    info.name
                );
            }
            return container.open_volume(&info);
        }
        available.extend(vols.into_iter().map(|v| v.name));
    }

    anyhow::bail!("volume {vol_name:?} not found. Available volumes: {available:?}");
}

fn cmd_apfs_ls<S, F>(open: F, vol_name: &str, path: &str) -> anyhow::Result<()>
where
    S: BlockSource + 'static,
    F: Fn() -> anyhow::Result<S>,
{
    let mut fs = find_apfs_volume(open, vol_name)?;
    let entries = fs.list_dir(path)?;
    println!("{path}  ({} entries)", entries.len());
    if entries.is_empty() {
        println!("(empty)");
        return Ok(());
    }
    println!("{:<6}  {:>14}  NAME", "KIND", "SIZE");
    for e in entries {
        println!(
            "{:<6}  {:>14}  {}",
            if e.is_dir { "DIR" } else { "FILE" },
            e.size_bytes,
            e.name,
        );
    }
    Ok(())
}

fn cmd_apfs_pull<S, F>(
    open: F,
    vol_name: &str,
    src: &str,
    dst_dir: &Path,
    skip_existing: bool,
) -> anyhow::Result<()>
where
    S: BlockSource + 'static,
    F: Fn() -> anyhow::Result<S>,
{
    let mut fs = find_apfs_volume(open, vol_name)?;
    let start = Instant::now();
    let opts = PullOptions {
        skip_existing,
        ..PullOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let mut files_seen = 0u64;
    let mut last = Instant::now();
    let mut on_event = |ev: PullEvent| match ev {
        PullEvent::Error { src, message } => eprintln!("  ! {src}: {message}"),
        PullEvent::FinishedFile { .. } => {
            files_seen += 1;
            if last.elapsed().as_millis() >= 250 {
                last = Instant::now();
                eprint!("  {files_seen} files done\r");
                let _ = std::io::stderr().flush();
            }
        }
        _ => {}
    };
    let stats = pull_tree(&mut fs, src, dst_dir, &opts, &cancel, &mut on_event)?;
    let secs = start.elapsed().as_secs_f64();
    let mb = stats.bytes as f64 / (1u64 << 20) as f64;
    eprintln!(
        "\npull complete: {} files / {} dirs / {:.2} MiB in {:.1}s ({:.1} MiB/s), {} skipped, {} errors",
        stats.files,
        stats.dirs,
        mb,
        secs,
        if secs > 0.0 { mb / secs } else { 0.0 },
        stats.skipped,
        stats.errors,
    );
    Ok(())
}

/// Measure cold vs warm directory listing on one APFS volume — the
/// listing reads every child's inode for its size, so this exercises
/// the children/path/size/node caches end to end.
fn cmd_apfs_bench<S, F>(open: F, vol_name: &str, path: &str) -> anyhow::Result<()>
where
    S: BlockSource + 'static,
    F: Fn() -> anyhow::Result<S>,
{
    let mut fs = find_apfs_volume(open, vol_name)?;

    let t0 = Instant::now();
    let entries = fs.list_dir(path)?;
    let cold = t0.elapsed();
    let n = entries.len();

    let t1 = Instant::now();
    let _ = fs.list_dir(path)?;
    let warm = t1.elapsed();

    let speedup = cold.as_secs_f64() / warm.as_secs_f64().max(1e-9);
    println!(
        "list_dir({path}) over {n} entries:\n  \
         cold {:.2} ms\n  warm {:.2} ms  ({:.0}x faster)",
        cold.as_secs_f64() * 1000.0,
        warm.as_secs_f64() * 1000.0,
        speedup,
    );
    Ok(())
}

fn cmd_apfs_cat<S, F>(open: F, vol_name: &str, path: &str) -> anyhow::Result<()>
where
    S: BlockSource + 'static,
    F: Fn() -> anyhow::Result<S>,
{
    let mut fs = find_apfs_volume(open, vol_name)?;
    let st = fs.stat(path)?;
    if st.is_dir {
        anyhow::bail!("{path} is a directory");
    }
    let mut buf = vec![0u8; 1024 * 1024];
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

fn print_container<S: BlockSource>(
    label: &str,
    mut container: ApfsContainer<S>,
) -> anyhow::Result<()> {
    let sb = container.superblock();
    let block_size = container.block_size();
    let total_gib = sb.block_count as f64 * block_size as f64 / (1u64 << 30) as f64;
    println!(
        "APFS container {label}: block_size={} block_count={} ({:.2} GiB)",
        block_size, sb.block_count, total_gib,
    );

    let volumes = container.volumes()?;
    if volumes.is_empty() {
        println!("  (no volumes)");
        return Ok(());
    }
    for v in &volumes {
        let used_gib = v.alloc_count as f64 * block_size as f64 / (1u64 << 30) as f64;
        println!(
            "  • {:<28} role={:<9} {} files / {} dirs · {:.2} GiB used{}{}",
            v.name,
            v.role_name(),
            v.num_files,
            v.num_directories,
            used_gib,
            if v.case_insensitive {
                " · case-insensitive"
            } else {
                ""
            },
            if v.encrypted { "  [ENCRYPTED]" } else { "" },
        );
    }
    Ok(())
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
            let skip_existing = rest.contains(&"--skip-existing");
            let positional: Vec<&str> = rest
                .iter()
                .copied()
                .filter(|a| !a.starts_with("--"))
                .collect();
            let src = positional
                .first()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("pull needs a source path"))?;
            let dst = positional
                .get(1)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("pull needs a destination directory"))?;
            cmd_pull(fs, src, Path::new(dst), skip_existing)
        }
        "bench" => {
            let path = rest.first().copied().unwrap_or("/");
            cmd_bench(fs, path)
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
           applesauce-cat <image> pull <src> <dst-dir> [--skip-existing]  # recursive copy off the volume\n  \
           applesauce-cat <image> bench [path]          # perf smoke test\n  \
           applesauce-cat <image> apfs                  # list APFS containers + volumes\n  \
           applesauce-cat <image> apfs-ls <vol> [path]  # list a dir in an APFS volume\n  \
           applesauce-cat --disk N [...]                # read \\\\.\\PhysicalDriveN (Admin)\n  \
           applesauce-cat --disk N apfs                 # enumerate APFS volumes on a disk\n  \
           applesauce-cat --disk N apfs-ls <vol> [path] # list a dir in an APFS volume\n  \
           applesauce-cat --disk N apfs-cat <vol> <path> # dump an APFS file to stdout\n  \
           applesauce-cat --disk N apfs-pull <vol> <src> <dst> [--skip-existing]  # recover APFS files\n\
         \n\
         pull is restartable: it skips destination files whose size and\n\
         mtime already match the source, and writes each in-flight file\n\
         to <name>.applesauce-partial before renaming on success.\n\
         --skip-existing widens the skip rule to any file already at the\n\
         destination path, regardless of size or mtime.\n\
         \n\
         bench measures the cost of metadata + read operations under a\n\
         directory: cold + warm `stat` on each child, one `list_dir`,\n\
         and a sequential read of the first regular file found.",
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

/// Perf smoke test: enumerate `path` once (cold), then `stat` each
/// child twice (cold + warm) and report median latencies, then do one
/// `list_dir` warm and one 4 MiB sequential read on the first regular
/// file. Lets us baseline cache wins between releases.
fn cmd_bench<S: std::io::Read + std::io::Seek + Send>(
    fs: &mut Hfsplus<S>,
    path: &str,
) -> anyhow::Result<()> {
    use std::time::Instant;

    // Cold list_dir.
    let t0 = Instant::now();
    let entries = fs.list_dir(path)?;
    let cold_list_us = t0.elapsed().as_micros();
    let n = entries.len();
    println!(
        "list_dir({path}) cold:    {} entries in {:.2} ms ({:.0} entries/ms)",
        n,
        cold_list_us as f64 / 1000.0,
        n as f64 / (cold_list_us.max(1) as f64 / 1000.0),
    );

    // Warm list_dir — exercises children_cache.
    let t0 = Instant::now();
    let _ = fs.list_dir(path)?;
    let warm_list_us = t0.elapsed().as_micros();
    println!(
        "list_dir({path}) warm:    {} entries in {:.2} ms (speedup {:.1}x)",
        n,
        warm_list_us as f64 / 1000.0,
        cold_list_us as f64 / warm_list_us.max(1) as f64,
    );

    // Cold stat each child (different paths — exercises path resolution).
    let mut cold_stats_us: Vec<u128> = Vec::with_capacity(n);
    for e in &entries {
        let child = if path == "/" {
            format!("/{}", e.name)
        } else {
            format!("{}/{}", path, e.name)
        };
        let t0 = Instant::now();
        let _ = fs.stat(&child);
        cold_stats_us.push(t0.elapsed().as_micros());
    }
    if !cold_stats_us.is_empty() {
        cold_stats_us.sort_unstable();
        let median = cold_stats_us[cold_stats_us.len() / 2];
        let total: u128 = cold_stats_us.iter().sum();
        println!(
            "stat (cold) over {n} children: median {median} µs, total {:.2} ms",
            total as f64 / 1000.0,
        );
    }

    // Warm stat (each path is now in path_cache).
    let mut warm_stats_us: Vec<u128> = Vec::with_capacity(n);
    for e in &entries {
        let child = if path == "/" {
            format!("/{}", e.name)
        } else {
            format!("{}/{}", path, e.name)
        };
        let t0 = Instant::now();
        let _ = fs.stat(&child);
        warm_stats_us.push(t0.elapsed().as_micros());
    }
    if !warm_stats_us.is_empty() {
        warm_stats_us.sort_unstable();
        let median = warm_stats_us[warm_stats_us.len() / 2];
        let total: u128 = warm_stats_us.iter().sum();
        println!(
            "stat (warm) over {n} children: median {median} µs, total {:.2} ms",
            total as f64 / 1000.0,
        );
    }

    // Sequential read of the first regular file we find.
    if let Some(file) = entries.iter().find(|e| !e.is_dir && e.size_bytes > 0) {
        let target = if path == "/" {
            format!("/{}", file.name)
        } else {
            format!("{}/{}", path, file.name)
        };
        let want = (4 * 1024 * 1024).min(file.size_bytes) as usize;
        let mut buf = vec![0u8; 65_536];
        let t0 = Instant::now();
        let mut offset = 0u64;
        let mut got = 0usize;
        while got < want {
            let n = fs.read_file_range(&target, offset, &mut buf)?;
            if n == 0 {
                break;
            }
            offset += n as u64;
            got += n;
        }
        let elapsed = t0.elapsed();
        let mib = got as f64 / (1024.0 * 1024.0);
        let mibps = mib / elapsed.as_secs_f64().max(0.000_001);
        println!(
            "read({target}) {} bytes ({:.2} MiB) in {:.2} ms ({:.1} MiB/s)",
            got,
            mib,
            elapsed.as_secs_f64() * 1000.0,
            mibps,
        );
    } else {
        println!("(no regular file in {path}, skipping read benchmark)");
    }

    Ok(())
}

fn cmd_pull<S: std::io::Read + std::io::Seek + Send>(
    fs: &mut Hfsplus<S>,
    src: &str,
    dst_dir: &Path,
    skip_existing: bool,
) -> anyhow::Result<()> {
    let start = Instant::now();
    let opts = PullOptions {
        skip_existing,
        ..PullOptions::default()
    };
    let cancel = AtomicBool::new(false);

    let mut files_seen: u64 = 0;
    let mut last_progress = Instant::now();
    let mut on_event = |ev: PullEvent| match ev {
        PullEvent::Error { src, message } => eprintln!("  ! {src}: {message}"),
        PullEvent::SkippedPrivate { name, .. } => {
            eprintln!("  - skipped private dir: {name}");
        }
        PullEvent::SkippedExisting { src, .. } => {
            eprintln!("  = skip (match): {src}");
        }
        PullEvent::FinishedFile { bytes_written, .. } => {
            files_seen += 1;
            if last_progress.elapsed().as_millis() >= 250 {
                last_progress = Instant::now();
                eprint!(
                    "  {} files done, last {} bytes\r",
                    files_seen, bytes_written
                );
                let _ = std::io::stderr().flush();
            }
        }
        _ => {}
    };

    let stats = pull_tree(fs, src, dst_dir, &opts, &cancel, &mut on_event)?;

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
