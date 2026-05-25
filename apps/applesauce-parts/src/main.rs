//! applesauce-parts — list partitions on a Mac disk or image, and the
//! volumes inside any APFS containers.
//!
//! Usage:
//!   applesauce-parts <image-file>      list partitions in an image
//!   applesauce-parts --disk N          list partitions on \\.\PhysicalDriveN (Windows, admin)
//!   applesauce-parts --list-disks      enumerate physical disks on the system

use std::env;
use std::process::ExitCode;

use block_source::image::ImageFile;
use block_source::partition::{self, Partition, APPLE_APFS_CONTAINER_GUID};
use block_source::window::Window;
use block_source::BlockSource;
use fs_core::apfs::ApfsContainer;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("applesauce-parts: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }

    if args[0] == "--list-disks" {
        return cmd_list_disks();
    }

    if args[0] == "--disk" {
        let n: u32 = args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("--disk requires a drive number"))?
            .parse()?;
        return cmd_disk(n);
    }

    cmd_image(&args[0])
}

fn print_usage() {
    eprintln!(
        "applesauce-parts {}\n\
         \n\
         List partitions on a Mac disk or image, and the volumes inside\n\
         any APFS containers.\n\
         \n\
         USAGE:\n  \
           applesauce-parts <image-file>\n  \
           applesauce-parts --disk N        # Windows, requires Administrator\n  \
           applesauce-parts --list-disks    # Windows, requires Administrator",
        env!("CARGO_PKG_VERSION"),
    );
}

fn cmd_list_disks() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        let disks = block_source::physical::enumerate();
        if disks.is_empty() {
            eprintln!("No physical disks visible. Are you running as Administrator?");
            return Ok(());
        }
        println!("{:<6}  {:>18}", "DISK", "BYTES");
        for d in disks {
            println!("PhysicalDrive{:<2}  {:>18}", d.drive_number, d.length_bytes);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("--list-disks is Windows-only")
    }
}

fn cmd_disk(_n: u32) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        report(move || Ok(block_source::physical::PhysicalDisk::open(_n)?))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("--disk is Windows-only")
    }
}

fn cmd_image(path: &str) -> anyhow::Result<()> {
    let path = path.to_string();
    report(move || Ok(ImageFile::open(&path)?))
}

/// Print the partition table, then summarize any APFS containers.
/// `open` yields a fresh source per call (probe consumes one; each
/// APFS container is windowed from its own).
fn report<S, F>(open: F) -> anyhow::Result<()>
where
    S: BlockSource + 'static,
    F: Fn() -> anyhow::Result<S>,
{
    let mut src = open()?;
    print_header(src.len_bytes());
    let parts = partition::probe(&mut src)?;
    drop(src);
    print_partitions(&parts);
    for p in parts.iter().filter(|p| is_apfs_container(p)) {
        summarize_apfs(p, &open);
    }
    Ok(())
}

fn is_apfs_container(p: &Partition) -> bool {
    matches!(
        p.type_id.as_str(),
        APPLE_APFS_CONTAINER_GUID | "Apple_APFS" | "Apple_APFS_Container"
    )
}

/// Open one APFS container partition and list its volumes. Errors are
/// printed and swallowed so one bad container doesn't abort the rest.
fn summarize_apfs<S, F>(p: &Partition, open: &F)
where
    S: BlockSource + 'static,
    F: Fn() -> anyhow::Result<S>,
{
    println!("\nAPFS container “{}”:", p.name);
    let result = (|| -> anyhow::Result<()> {
        let window = Window::new(open()?, p.start_byte, p.length_bytes)?;
        let mut container = ApfsContainer::open(window)?;
        let volumes = container.volumes()?;
        if volumes.is_empty() {
            println!("  (no volumes)");
            return Ok(());
        }
        for v in &volumes {
            let used_gib =
                v.alloc_count as f64 * container.block_size() as f64 / (1u64 << 30) as f64;
            println!(
                "  • {:<28} role={:<9} {} files / {} dirs · {:.2} GiB{}",
                v.name,
                v.role_name(),
                v.num_files,
                v.num_directories,
                used_gib,
                if v.encrypted { "  [ENCRYPTED]" } else { "" },
            );
        }
        Ok(())
    })();
    if let Err(e) = result {
        println!("  (could not read container: {e:#})");
    }
}

fn print_header(len: Option<u64>) {
    match len {
        Some(b) => println!(
            "disk size: {} bytes ({:.2} GiB)",
            b,
            b as f64 / (1u64 << 30) as f64
        ),
        None => println!("disk size: unknown"),
    }
}

fn print_partitions(parts: &[Partition]) {
    if parts.is_empty() {
        println!("(no recognized partition table)");
        return;
    }
    println!(
        "{:<3}  {:<26}  {:<40}  {:>16}  {:>16}  MAC?",
        "#", "NAME", "TYPE", "START", "LENGTH"
    );
    for (i, p) in parts.iter().enumerate() {
        println!(
            "{:<3}  {:<26.26}  {:<40.40}  {:>16}  {:>16}  {}",
            i,
            p.name,
            p.type_id,
            p.start_byte,
            p.length_bytes,
            if p.is_mac_filesystem() { "yes" } else { "" },
        );
    }
}
