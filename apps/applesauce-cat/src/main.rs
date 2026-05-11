//! applesauce-cat — exercise the HFS+ reader against a disk image or
//! a physical Mac drive plugged into Windows.
//!
//! Usage:
//!   applesauce-cat <image>                          — volume info
//!   applesauce-cat <image> ls [path]                — list a directory
//!   applesauce-cat <image> cat <path>               — dump a file
//!
//!   applesauce-cat --disk N                         — volume info
//!   applesauce-cat --disk N ls [path]               — list a directory
//!   applesauce-cat --disk N cat <path>              — dump a file
//!
//! If the source contains a partition table, the first Mac-typed
//! volume is auto-selected. If there's no partition table, the source
//! is treated as a bare HFS+ volume.

use std::env;
use std::io::Write;
use std::process::ExitCode;

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
            let arg = args.get(3);
            let source = block_source::physical::PhysicalDisk::open(n)?;
            let (volume_offset, mut fs) = open_first_hfsplus(source)?;
            return dispatch(command, arg, &mut fs, volume_offset);
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("--disk is Windows-only");
        }
    }

    let image_path = &args[0];
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("info");
    let arg = args.get(2);

    let source = ImageFile::open(image_path)?;
    let (volume_offset, mut fs) = open_first_hfsplus(source)?;
    dispatch(command, arg, &mut fs, volume_offset)
}

fn dispatch<S: BlockSource + 'static>(
    command: &str,
    arg: Option<&String>,
    fs: &mut Hfsplus<Window<S>>,
    volume_offset: u64,
) -> anyhow::Result<()> {
    match command {
        "info" => cmd_info(fs, volume_offset),
        "ls" => cmd_ls(fs, arg.map(|s| s.as_str()).unwrap_or("/")),
        "cat" => {
            let path = arg.ok_or_else(|| anyhow::anyhow!("cat needs a path"))?;
            cmd_cat(fs, path)
        }
        other => anyhow::bail!("unknown subcommand {other:?}"),
    }
}

fn print_usage() {
    eprintln!(
        "applesauce-cat {}\n\
         \n\
         USAGE:\n  \
           applesauce-cat <image>                   # volume info\n  \
           applesauce-cat <image> ls [path]         # list directory (default: /)\n  \
           applesauce-cat <image> cat <path>        # dump file to stdout\n  \
           applesauce-cat --disk N [...]            # read \\\\.\\PhysicalDriveN (Admin)",
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
        .find(|p| matches!(p.scheme, PartitionScheme::Gpt | PartitionScheme::Apm)
            && p.is_mac_filesystem())
        .cloned();

    let (start, length) = match chosen {
        Some(p) => (p.start_byte, p.length_bytes),
        None => {
            // No partition table — treat the source as a bare HFS+ volume.
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
    println!("{:<8}  {:>14}  {}", "KIND", "SIZE", "NAME");
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
