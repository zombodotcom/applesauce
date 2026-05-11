//! applesauce-parts — list partitions on a Mac disk or image.
//!
//! Usage:
//!   applesauce-parts <image-file>      list partitions in an image
//!   applesauce-parts --disk N          list partitions on \\.\PhysicalDriveN (Windows, admin)
//!   applesauce-parts --list-disks      enumerate physical disks on the system

use std::env;
use std::process::ExitCode;

use block_source::image::ImageFile;
use block_source::partition::{self, Partition};
use block_source::BlockSource;

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
         List partitions on a Mac disk or image.\n\
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
            eprintln!(
                "No physical disks visible. Are you running as Administrator?"
            );
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
        let mut src = block_source::physical::PhysicalDisk::open(_n)?;
        print_header(src.len_bytes());
        let parts = partition::probe(&mut src)?;
        print_partitions(&parts);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("--disk is Windows-only")
    }
}

fn cmd_image(path: &str) -> anyhow::Result<()> {
    let mut src = ImageFile::open(path)?;
    print_header(src.len_bytes());
    let parts = partition::probe(&mut src)?;
    print_partitions(&parts);
    Ok(())
}

fn print_header(len: Option<u64>) {
    match len {
        Some(b) => println!("disk size: {} bytes ({:.2} GiB)", b, b as f64 / (1u64 << 30) as f64),
        None => println!("disk size: unknown"),
    }
}

fn print_partitions(parts: &[Partition]) {
    if parts.is_empty() {
        println!("(no recognized partition table)");
        return;
    }
    println!(
        "{:<3}  {:<26}  {:<40}  {:>16}  {:>16}  {}",
        "#", "NAME", "TYPE", "START", "LENGTH", "MAC?"
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
