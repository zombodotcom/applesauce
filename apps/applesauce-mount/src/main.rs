//! applesauce-mount — mount an HFS+ volume (Mac drive or image) as a
//! Windows drive letter. Read-only.
//!
//! Usage:
//!   applesauce-mount install                     # register with WinFsp.Launcher
//!   applesauce-mount uninstall                   # remove from WinFsp.Launcher
//!   applesauce-mount --disk N <drive-letter>     # e.g. --disk 4 Z:
//!   applesauce-mount <image>   <drive-letter>    # mount an image file
//!
//! Press Ctrl-C to unmount and exit.
//!
//! ## How "install" makes the drive visible to normal Explorer
//!
//! Raw physical-disk reads require Administrator on Windows; once
//! mounted, the resulting drive letter is normally scoped to the
//! elevated session (Windows' UAC "linked connections" split). To
//! avoid making the user run Explorer elevated, `install` writes a
//! one-time registry entry under `HKLM\SOFTWARE\WinFsp\Services\applesauce`
//! that registers this binary with the WinFsp.Launcher SYSTEM
//! service. From then on any unprivileged user can run
//! `launchctl-x64.exe start applesauce <id> <disk> <letter>` and the
//! launcher spawns us as SYSTEM — drive letter visible everywhere.

#[cfg(not(windows))]
fn main() {
    eprintln!("applesauce-mount is Windows-only.");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    win::run()
}

#[cfg(windows)]
mod win {
    use std::env;
    use std::process::ExitCode;
    use std::sync::mpsc;

    use block_source::image::ImageFile;
    use block_source::partition::{self, Partition, PartitionScheme};
    use block_source::window::Window;
    use block_source::BlockSource;
    use fs_core::apfs::ApfsContainer;
    use fs_core::hfsplus::Hfsplus;

    pub fn run() -> ExitCode {
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

        let result = match args[0].as_str() {
            "install" => install_launcher_service(),
            "uninstall" => uninstall_launcher_service(),
            _ => drive(&args),
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("applesauce-mount: {e:#}");
                ExitCode::FAILURE
            }
        }
    }

    /// Registry path the WinFsp.Launcher service reads.
    const REG_BASE: &str = r"HKLM\SOFTWARE\WinFsp\Services\applesauce";

    fn install_launcher_service() -> anyhow::Result<()> {
        let exe = std::env::current_exe()?;
        let exe_str = exe.to_string_lossy().to_string();

        // CommandLine template substituted by the launcher:
        //   %1 = disk number, %2 = drive letter,
        //   %3 = partition byte offset, %4 = volume selector
        //        ("hfs" for HFS+, or an APFS volume index)
        // launchctl invocation:
        //   launchctl-x64 start applesauce <id> <disk> <letter> <offset> <sel>
        let cmdline = "--disk %1 %2 %3 %4";

        reg_set(REG_BASE, "Executable", RegType::Sz, &exe_str)?;
        reg_set(REG_BASE, "CommandLine", RegType::Sz, cmdline)?;
        reg_set(REG_BASE, "JobControl", RegType::Dword, "1")?;
        // No Security key — defaults to allow LocalSystem + admins +
        // built-in Users to invoke. That's what we want: any signed-in
        // user can call launchctl-x64 start applesauce.
        // (Default SDDL: D:P(A;;RPWPLC;;;WD) — World, Read/Write/Launch.)
        reg_set(REG_BASE, "Security", RegType::Sz, "D:P(A;;RPWPLC;;;WD)")?;

        eprintln!("Registered '{exe_str}' under HKLM\\SOFTWARE\\WinFsp\\Services\\applesauce.");
        eprintln!("Anyone can now mount without admin:");
        eprintln!(r#"  launchctl-x64.exe start applesauce <id> <disk> <letter>"#);
        eprintln!(r#"e.g.  launchctl-x64.exe start applesauce mac4 4 Z:"#);
        Ok(())
    }

    fn uninstall_launcher_service() -> anyhow::Result<()> {
        let status = std::process::Command::new("reg")
            .args(["delete", REG_BASE, "/f", "/reg:32"])
            .status()?;
        if !status.success() {
            anyhow::bail!("reg delete failed (exit {})", status);
        }
        eprintln!("Unregistered applesauce from WinFsp.Launcher.");
        Ok(())
    }

    enum RegType {
        Sz,
        Dword,
    }

    fn reg_set(key: &str, name: &str, ty: RegType, value: &str) -> anyhow::Result<()> {
        let ty_arg = match ty {
            RegType::Sz => "REG_SZ",
            RegType::Dword => "REG_DWORD",
        };
        let status = std::process::Command::new("reg")
            .args([
                "add", key, "/v", name, "/t", ty_arg, "/d", value, "/f", "/reg:32",
            ])
            .status()?;
        if !status.success() {
            anyhow::bail!(
                "reg add {key}\\{name} failed (exit {status}) — run from an elevated shell"
            );
        }
        Ok(())
    }

    fn drive(args: &[String]) -> anyhow::Result<()> {
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
                // Optional explicit selection: <offset> <selector>. When
                // absent, fall back to auto-detecting the first Mac fs.
                let host = match (args.get(3), args.get(4)) {
                    (Some(offset), Some(sel)) => {
                        let offset: u64 = offset.parse()?;
                        open_and_mount_selected(source, offset, sel, &mountpoint)?
                    }
                    _ => open_and_mount(source, &mountpoint)?,
                };
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
    }

    fn print_usage() {
        eprintln!(
            "applesauce-mount {}\n\
             \n\
             USAGE:\n  \
               applesauce-mount install                   # register with WinFsp.Launcher (admin, one-time)\n  \
               applesauce-mount uninstall                 # unregister (admin)\n  \
               applesauce-mount --disk N <drive-letter>   # direct mount: --disk 4 Z: (admin)\n  \
               applesauce-mount <image>   <drive-letter>  # mount image file\n\
             \n\
             After `install`, any user can mount without admin via:\n  \
               launchctl-x64.exe start applesauce <id> <disk-num> <letter>\n  \
               launchctl-x64.exe stop  applesauce <id>\n\
             \n\
             Requires WinFsp (https://winfsp.dev/).",
            env!("CARGO_PKG_VERSION"),
        );
    }

    /// Mount a specific volume identified by its partition `offset` and
    /// a `selector`: `"hfs"` for an HFS+ partition, or an APFS volume
    /// index (the position within its container's volume list). The
    /// partition length is recovered by re-probing the table.
    fn open_and_mount_selected<S: BlockSource + 'static>(
        mut source: S,
        offset: u64,
        selector: &str,
        mountpoint: &str,
    ) -> anyhow::Result<winfsp_bridge::MountedHost> {
        let parts = partition::probe(&mut source)?;
        let part = parts
            .iter()
            .find(|p| p.start_byte == offset)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no partition at offset {offset}"))?;
        let (start, length) = (part.start_byte, part.length_bytes);
        let window = Window::new(source, start, length)?;

        if selector.eq_ignore_ascii_case("hfs") {
            let fs = Hfsplus::open(window, 0)?;
            return winfsp_bridge::mount(fs, length, mountpoint);
        }

        // APFS: selector is the volume index within the container.
        let index: usize = selector
            .parse()
            .map_err(|_| anyhow::anyhow!("bad APFS volume selector {selector:?}"))?;
        let mut container = ApfsContainer::open(window)?;
        let vols = container.volumes()?;
        let info = vols
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("APFS volume index {index} out of range"))?;
        if info.encrypted {
            anyhow::bail!(
                "APFS volume “{}” is FileVault-encrypted; mounting needs the key",
                info.name
            );
        }
        let vol = container.open_volume(&info)?;
        winfsp_bridge::mount(vol, length, mountpoint)
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
