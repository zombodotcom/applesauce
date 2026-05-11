//! applesauce-mount — mount an HFS+ volume (Mac drive or image) as a
//! Windows drive letter. Read-only.
//!
//! Usage:
//!   applesauce-mount --disk N <drive-letter>     # e.g. --disk 4 Z:
//!   applesauce-mount <image>   <drive-letter>    # mount an image file
//!
//! Press Ctrl-C to unmount and exit.

#[cfg(not(windows))]
fn main() {
    eprintln!("applesauce-mount is Windows-only.");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::env;
    use std::process::ExitCode;
    use std::sync::mpsc;

    use block_source::image::ImageFile;
    use block_source::partition::{self, Partition, PartitionScheme};
    use block_source::window::Window;
    use block_source::BlockSource;
    use fs_core::hfsplus::Hfsplus;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    let result = (|| -> anyhow::Result<()> {
        let (mountpoint, host) = match args[0].as_str() {
            "--disk" => {
                let n: u32 = args
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("--disk requires a drive number"))?
                    .parse()?;
                let mountpoint = args
                    .get(2)
                    .ok_or_else(|| anyhow::anyhow!("missing drive letter (e.g. Z:)"))?
                    .clone();
                let source = block_source::physical::PhysicalDisk::open(n)?;
                let host = open_and_mount(source, &mountpoint)?;
                (mountpoint, host)
            }
            image_path => {
                let mountpoint = args
                    .get(1)
                    .ok_or_else(|| anyhow::anyhow!("missing drive letter (e.g. Z:)"))?
                    .clone();
                let source = ImageFile::open(image_path)?;
                let host = open_and_mount(source, &mountpoint)?;
                (mountpoint, host)
            }
        };

        eprintln!("mounted on {mountpoint}. Press Ctrl-C to unmount.");

        let (tx, rx) = mpsc::channel::<()>();
        ctrlc::set_handler(move || {
            let _ = tx.send(());
        })?;
        let _ = rx.recv();

        eprintln!("unmounting…");
        host.unmount();
        Ok(())
    })();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("applesauce-mount: {e:#}");
            ExitCode::FAILURE
        }
    }

    fn print_usage() {
        eprintln!(
            "applesauce-mount {}\n\
             \n\
             USAGE:\n  \
               applesauce-mount --disk N <drive-letter>   # e.g. --disk 4 Z:\n  \
               applesauce-mount <image>   <drive-letter>  # mount image file\n\
             \n\
             Requires WinFsp (https://winfsp.dev/) and Administrator for --disk.",
            env!("CARGO_PKG_VERSION"),
        );
    }

    fn open_and_mount<S: BlockSource + 'static>(
        mut source: S,
        mountpoint: &str,
    ) -> anyhow::Result<winfsp_bridge::MountedHost> {
        let parts = partition::probe(&mut source)?;

        let chosen: Option<Partition> = parts
            .iter()
            .find(|p| {
                matches!(p.scheme, PartitionScheme::Gpt | PartitionScheme::Apm)
                    && p.is_mac_filesystem()
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
        winfsp_bridge::mount(fs, length, mountpoint)
    }
}
