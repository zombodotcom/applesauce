//! applesauce — mount Mac drives as Windows drive letters.
//!
//! The GUI scans for HFS+/APFS-typed partitions on physical disks and
//! delegates mounting to the WinFsp.Launcher service via
//! `launchctl-x64.exe`. That way the mounted drive is visible to
//! every Explorer session (admin or not), and the GUI itself only
//! needs admin to read the raw disk partition tables — not to mount.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result<()> {
    // Always log to a file alongside the binary so we can diagnose
    // scan / mount issues without keeping a console open. Useful in
    // release where windows_subsystem detaches stdout.
    let log_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("applesauce.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("applesauce.log"));
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    let builder = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
    );
    match log_file {
        Some(f) => builder.with_writer(std::sync::Mutex::new(f)).init(),
        None => builder.init(),
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 520.0])
            .with_title("applesauce"),
        ..Default::default()
    };

    eframe::run_native(
        "applesauce",
        options,
        Box::new(|_cc| Ok(Box::new(app::App::new()))),
    )
}

#[cfg(not(windows))]
mod app {
    pub struct App;
    impl App {
        pub fn new() -> Self {
            Self
        }
    }
    impl eframe::App for App {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("applesauce");
                ui.label("applesauce only runs on Windows.");
            });
        }
    }
}

#[cfg(windows)]
mod app {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    use block_source::partition::{self, Partition, PartitionScheme};
    use block_source::physical::{self, DiskInfo, PhysicalDisk};
    use block_source::window::Window;
    use fs_core::hfsplus::Hfsplus;
    use fs_core::pull::{pull_tree, PullEvent, PullOptions, PullStats};
    use fs_core::MacFilesystem;

    /// Registry key the WinFsp.Launcher reads to find our mount binary.
    const SERVICE_REG: &str = r"HKLM\SOFTWARE\WOW6432Node\WinFsp\Services\applesauce";

    /// Service "class name" registered with WinFsp.Launcher.
    const SERVICE_CLASS: &str = "applesauce";

    /// A scanned Mac-typed partition we know how to mount.
    #[derive(Clone)]
    struct ScannedVolume {
        drive_number: u32,
        partition_label: String,
        volume_label: String,
        start_byte: u64,
        length_bytes: u64,
    }

    enum ScanResult {
        Done(Vec<ScannedVolume>),
    }

    enum LaunchResult {
        Ok {
            instance: String,
            mountpoint: String,
        },
        Err(String),
    }

    /// One drive we asked the launcher to mount.
    struct ActiveMount {
        instance: String,
        mountpoint: String,
    }

    /// Live state of an in-progress pull. The worker thread updates
    /// `files` / `bytes` atomically; the UI reads them every frame.
    struct PullProgress {
        files: Arc<AtomicU64>,
        bytes: Arc<AtomicU64>,
        skipped: Arc<AtomicU64>,
        errors: Arc<AtomicU64>,
        last_file: Arc<std::sync::Mutex<String>>,
        cancel: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
        rx: Receiver<PullDone>,
        dst_root: PathBuf,
        started_at: std::time::Instant,
    }

    struct PullDone {
        result: anyhow::Result<PullStats>,
    }

    pub struct App {
        volumes: Vec<ScannedVolume>,
        active: Vec<ActiveMount>,
        scanning: Option<(JoinHandle<()>, Receiver<ScanResult>)>,
        mounting: Option<(JoinHandle<()>, Receiver<LaunchResult>)>,
        pulling: Option<PullProgress>,
        // Per-row drive-letter selection.
        chosen_letter: std::collections::HashMap<(u32, u64), String>,
        // Per-row source path for Pull.
        pull_src: std::collections::HashMap<(u32, u64), String>,
        status: String,
        admin: bool,
        service_installed: bool,
        launchctl: Option<PathBuf>,
    }

    impl App {
        pub fn new() -> Self {
            let admin = is_admin();
            let service_installed = is_service_registered();
            let launchctl = find_launchctl();
            let mut me = Self {
                volumes: Vec::new(),
                active: Vec::new(),
                scanning: None,
                mounting: None,
                pulling: None,
                chosen_letter: Default::default(),
                pull_src: Default::default(),
                status: String::new(),
                admin,
                service_installed,
                launchctl,
            };
            if me.admin {
                me.start_scan();
            } else {
                me.status =
                    "Run as Administrator once to scan disks. Mounting itself is unprivileged."
                        .to_string();
            }
            // Pick up any mounts the launcher already knows about (e.g.
            // from a previous GUI session or a CLI launchctl call).
            me.refresh_active_mounts();
            me
        }

        fn start_scan(&mut self) {
            if self.scanning.is_some() {
                return;
            }
            let (tx, rx) = mpsc::channel();
            let handle = thread::spawn(move || {
                let res = scan_volumes();
                let _ = tx.send(res);
            });
            self.scanning = Some((handle, rx));
            self.status = "Scanning disks…".to_string();
        }

        fn poll_scan(&mut self) {
            // try_recv consumes — drain it here, then drop the channel.
            let result = self
                .scanning
                .as_ref()
                .and_then(|(_, rx)| rx.try_recv().ok());
            if let Some(res) = result {
                if let Some((handle, _rx)) = self.scanning.take() {
                    let _ = handle.join();
                }
                match res {
                    ScanResult::Done(v) => {
                        self.status = format!("Found {} Mac volume(s).", v.len());
                        self.volumes = v;
                    }
                }
            }
        }

        fn poll_mount(&mut self) {
            let result = self
                .mounting
                .as_ref()
                .and_then(|(_, rx)| rx.try_recv().ok());
            if let Some(res) = result {
                if let Some((handle, _rx)) = self.mounting.take() {
                    let _ = handle.join();
                }
                match res {
                    LaunchResult::Ok {
                        instance,
                        mountpoint,
                    } => {
                        self.status = format!("Mounted on {mountpoint}.");
                        self.active.push(ActiveMount {
                            instance,
                            mountpoint,
                        });
                    }
                    LaunchResult::Err(e) => {
                        self.status = format!("Mount failed: {e}");
                    }
                }
            }
        }

        fn start_mount(&mut self, vol_idx: usize, letter: String) {
            let Some(launchctl) = self.launchctl.clone() else {
                self.status = "launchctl-x64.exe not found — is WinFsp installed?".to_string();
                return;
            };
            let v = match self.volumes.get(vol_idx) {
                Some(v) => v,
                None => return,
            };
            let drive_number = v.drive_number;
            let instance = format!("disk{drive_number}-{}", letter.replace(':', ""));
            let mountpoint = letter;
            let (tx, rx) = mpsc::channel::<LaunchResult>();
            let instance_for_thread = instance.clone();
            let mountpoint_for_thread = mountpoint.clone();
            let handle = thread::spawn(move || {
                let out = Command::new(&launchctl)
                    .args([
                        "start",
                        SERVICE_CLASS,
                        &instance_for_thread,
                        &drive_number.to_string(),
                        &mountpoint_for_thread,
                    ])
                    .output();
                let res = match out {
                    Ok(o) if o.status.success() => LaunchResult::Ok {
                        instance: instance_for_thread,
                        mountpoint: mountpoint_for_thread,
                    },
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        let stdout = String::from_utf8_lossy(&o.stdout);
                        LaunchResult::Err(format!(
                            "launchctl exit {}: {}{}",
                            o.status,
                            stdout.trim(),
                            stderr.trim()
                        ))
                    }
                    Err(e) => LaunchResult::Err(format!("spawning launchctl: {e}")),
                };
                let _ = tx.send(res);
            });
            self.mounting = Some((handle, rx));
            self.status = format!("Mounting {instance} via WinFsp.Launcher…");
        }

        fn unmount(&mut self, idx: usize) {
            if idx >= self.active.len() {
                return;
            }
            let Some(launchctl) = self.launchctl.clone() else {
                self.status = "launchctl-x64.exe not found.".to_string();
                return;
            };
            let m = self.active.swap_remove(idx);
            let instance = m.instance.clone();
            let instance_for_thread = instance.clone();
            thread::spawn(move || {
                let _ = Command::new(&launchctl)
                    .args(["stop", SERVICE_CLASS, &instance_for_thread])
                    .output();
            });
            self.status = format!("Unmounting {instance}.");
        }

        /// Kick off a pull from `vol_idx`'s drive, copying `src_path`
        /// (POSIX path on the volume) into `dst_dir` on the local
        /// filesystem. Runs in a background thread; progress is
        /// readable via the [`PullProgress`] atomics every frame.
        fn start_pull(&mut self, vol_idx: usize, src_path: String, dst_dir: PathBuf) {
            if self.pulling.is_some() {
                self.status = "Another pull is already running.".to_string();
                return;
            }
            let v = match self.volumes.get(vol_idx) {
                Some(v) => v.clone(),
                None => return,
            };

            let files = Arc::new(AtomicU64::new(0));
            let bytes = Arc::new(AtomicU64::new(0));
            let skipped = Arc::new(AtomicU64::new(0));
            let errors = Arc::new(AtomicU64::new(0));
            let last_file = Arc::new(std::sync::Mutex::new(String::new()));
            let cancel = Arc::new(AtomicBool::new(false));

            let (tx, rx) = mpsc::channel::<PullDone>();
            let files_w = files.clone();
            let bytes_w = bytes.clone();
            let skipped_w = skipped.clone();
            let errors_w = errors.clone();
            let last_file_w = last_file.clone();
            let cancel_w = cancel.clone();
            let dst_dir_w = dst_dir.clone();
            let src_path_w = src_path.clone();

            let handle = thread::spawn(move || {
                let result = run_pull(
                    &v,
                    &src_path_w,
                    &dst_dir_w,
                    &cancel_w,
                    files_w,
                    bytes_w,
                    skipped_w,
                    errors_w,
                    last_file_w,
                );
                let _ = tx.send(PullDone { result });
            });

            // Drop-of-row "Pull into <name>" lands inside dst_dir.
            // pull_tree itself appends sanitize(leaf) as a subdir, so
            // for status purposes show dst_dir.
            self.pulling = Some(PullProgress {
                files,
                bytes,
                skipped,
                errors,
                last_file,
                cancel,
                handle: Some(handle),
                rx,
                dst_root: dst_dir,
                started_at: std::time::Instant::now(),
            });
            self.status = format!("Pulling {src_path}…");
        }

        fn poll_pull(&mut self) {
            let done_msg = self.pulling.as_ref().and_then(|p| p.rx.try_recv().ok());
            if let Some(done) = done_msg {
                if let Some(mut p) = self.pulling.take() {
                    if let Some(h) = p.handle.take() {
                        let _ = h.join();
                    }
                    match done.result {
                        Ok(stats) => {
                            let elapsed = p.started_at.elapsed();
                            let mb = stats.bytes as f64 / (1u64 << 20) as f64;
                            let mbps = if elapsed.as_secs_f64() > 0.0 {
                                mb / elapsed.as_secs_f64()
                            } else {
                                0.0
                            };
                            self.status = format!(
                                "Pull done: {} files / {} dirs / {:.2} MiB in {:.1}s ({:.1} MiB/s) → {}",
                                stats.files,
                                stats.dirs,
                                mb,
                                elapsed.as_secs_f64(),
                                mbps,
                                p.dst_root.display(),
                            );
                        }
                        Err(e) => self.status = format!("Pull failed: {e:#}"),
                    }
                }
            }
        }

        fn cancel_pull(&mut self) {
            if let Some(p) = &self.pulling {
                p.cancel.store(true, Ordering::Relaxed);
                self.status = "Cancelling pull…".to_string();
            }
        }

        fn refresh_active_mounts(&mut self) {
            let Some(launchctl) = &self.launchctl else {
                return;
            };
            let Ok(out) = Command::new(launchctl).arg("list").output() else {
                return;
            };
            if !out.status.success() {
                return;
            }
            let text = String::from_utf8_lossy(&out.stdout);
            self.active.clear();
            for line in text.lines() {
                // launchctl list output is `OK<NL><class> <instance>...`
                // We grab lines starting with our class name.
                let line = line.trim();
                if let Some(rest) = line.strip_prefix(&format!("{SERVICE_CLASS} ")) {
                    let inst = rest.split_whitespace().next().unwrap_or(rest.trim());
                    // We don't know the mountpoint from launchctl list,
                    // so we show the instance label only.
                    self.active.push(ActiveMount {
                        instance: inst.to_string(),
                        mountpoint: "(active)".to_string(),
                    });
                }
            }
        }

        fn install_service(&mut self) {
            // Re-launch ourselves via the mount binary with UAC. The
            // user gets exactly one consent prompt.
            let exe = match locate_mount_binary() {
                Some(p) => p,
                None => {
                    self.status =
                        "applesauce-mount.exe not found next to applesauce.exe.".to_string();
                    return;
                }
            };
            match runas(&exe, &["install"]) {
                Ok(()) => {
                    self.status =
                        "Service registered. You can mount without admin now.".to_string();
                    self.service_installed = is_service_registered();
                }
                Err(e) => self.status = format!("Install failed: {e}"),
            }
        }
    }

    impl eframe::App for App {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.poll_scan();
            self.poll_mount();
            self.poll_pull();
            if self.scanning.is_some() || self.mounting.is_some() || self.pulling.is_some() {
                ctx.request_repaint();
            }

            egui::TopBottomPanel::top("top").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("applesauce");
                    ui.label("— mount Mac drives as Windows drive letters");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                self.scanning.is_none() && self.admin,
                                egui::Button::new("Rescan"),
                            )
                            .clicked()
                        {
                            self.start_scan();
                        }
                    });
                });
            });

            egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
                if !self.service_installed {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 120, 0),
                            "Service not registered. Click → ",
                        );
                        if ui.button("Install service (UAC prompt)").clicked() {
                            self.install_service();
                        }
                    });
                }
                if !self.admin {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 120, 0),
                        "Scanning physical disks needs Administrator. Mounting itself doesn't.",
                    );
                }
                ui.label(&self.status);
            });

            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Mac volumes");
                if self.volumes.is_empty() && self.scanning.is_none() && self.admin {
                    ui.label("No HFS+ volumes detected. Plug in a Mac drive, then click Rescan.");
                }

                let busy_mounting = self.mounting.is_some();
                let busy_pulling = self.pulling.is_some();
                let active_letters: std::collections::HashSet<String> =
                    self.active.iter().map(|m| m.mountpoint.clone()).collect();

                let mut mount_request: Option<(usize, String)> = None;
                let mut pull_request: Option<(usize, String)> = None;

                for (i, v) in self.volumes.iter().enumerate() {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            let title = if v.volume_label.is_empty() {
                                v.partition_label.as_str()
                            } else {
                                v.volume_label.as_str()
                            };
                            ui.strong(title);
                            ui.small(format!(
                                "Disk {} · partition “{}” · {:.2} GiB",
                                v.drive_number,
                                v.partition_label,
                                v.length_bytes as f64 / (1u64 << 30) as f64
                            ));
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let key = (v.drive_number, v.start_byte);
                            let already_at = self.active.iter().find(|m| {
                                m.instance.starts_with(&format!("disk{}-", v.drive_number))
                            });

                            // Pull controls (right-most group).
                            let src = self
                                .pull_src
                                .entry(key)
                                .or_insert_with(|| "/Users".to_string());
                            if ui
                                .add_enabled(
                                    !busy_pulling && self.admin,
                                    egui::Button::new("Pull…"),
                                )
                                .on_hover_text(
                                    "Recursively copy the source path off this volume into \
                                     a destination folder you pick. Restartable: skips files \
                                     whose size + mtime already match.",
                                )
                                .clicked()
                            {
                                pull_request = Some((i, src.clone()));
                            }
                            ui.add(
                                egui::TextEdit::singleline(src)
                                    .desired_width(160.0)
                                    .hint_text("source path on volume"),
                            );

                            ui.separator();

                            // Mount controls.
                            if let Some(m) = already_at {
                                ui.add_enabled(
                                    false,
                                    egui::Button::new(format!("Mounted ({})", m.mountpoint)),
                                );
                            } else {
                                let chosen = self.chosen_letter.entry(key).or_insert_with(|| {
                                    first_free_letter(&active_letters)
                                        .unwrap_or_else(|| "Z:".to_string())
                                });

                                if ui
                                    .add_enabled(
                                        !busy_mounting
                                            && self.service_installed
                                            && self.launchctl.is_some(),
                                        egui::Button::new("Mount"),
                                    )
                                    .clicked()
                                {
                                    mount_request = Some((i, chosen.clone()));
                                }
                                egui::ComboBox::from_id_salt(("letter", key))
                                    .selected_text(chosen.as_str())
                                    .show_ui(ui, |ui| {
                                        for letter in candidate_letters(&active_letters) {
                                            ui.selectable_value(chosen, letter.clone(), letter);
                                        }
                                    });
                            }
                        });
                    });
                }

                if !self.active.is_empty() {
                    ui.add_space(12.0);
                    ui.heading("Active mounts");
                    let mut to_unmount: Option<usize> = None;
                    for (i, m) in self.active.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(format!("{}  ({})", m.mountpoint, m.instance));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Unmount").clicked() {
                                        to_unmount = Some(i);
                                    }
                                },
                            );
                        });
                    }
                    if let Some(i) = to_unmount {
                        self.unmount(i);
                    }
                }

                if let Some((idx, letter)) = mount_request {
                    self.start_mount(idx, letter);
                }
                if let Some((idx, src)) = pull_request {
                    // rfd's pick_folder is blocking. It pumps the
                    // native dialog, but our egui frame finishes and
                    // repaints once it returns.
                    if let Some(dst) = rfd::FileDialog::new()
                        .set_title("Pull destination folder")
                        .pick_folder()
                    {
                        self.start_pull(idx, src, dst);
                    }
                }

                // In-progress pull panel.
                if let Some(p) = &self.pulling {
                    ctx.request_repaint(); // animate counters
                    ui.add_space(12.0);
                    ui.heading("Pull in progress");
                    let files = p.files.load(Ordering::Relaxed);
                    let bytes = p.bytes.load(Ordering::Relaxed);
                    let skipped = p.skipped.load(Ordering::Relaxed);
                    let errors = p.errors.load(Ordering::Relaxed);
                    let mb = bytes as f64 / (1u64 << 20) as f64;
                    let elapsed = p.started_at.elapsed();
                    let mbps = if elapsed.as_secs_f64() > 0.0 {
                        mb / elapsed.as_secs_f64()
                    } else {
                        0.0
                    };
                    ui.label(format!("→ {}", p.dst_root.display()));
                    ui.label(format!(
                        "{files} files · {mb:.2} MiB · {mbps:.1} MiB/s · \
                         {skipped} skipped · {errors} errors"
                    ));
                    let last = p
                        .last_file
                        .lock()
                        .ok()
                        .map(|g| g.clone())
                        .unwrap_or_default();
                    if !last.is_empty() {
                        ui.small(format!("last: {last}"));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.cancel_pull();
                        }
                    });
                }
            });
        }
    }

    /// Open `vol`'s underlying disk, mount it as an HFS+ source, and
    /// run [`pull_tree`] from `src_path` into `dst_dir`. The four
    /// `AtomicU64`s are bumped every progress event so the UI can read
    /// them per-frame.
    #[allow(clippy::too_many_arguments)]
    fn run_pull(
        vol: &ScannedVolume,
        src_path: &str,
        dst_dir: &std::path::Path,
        cancel: &AtomicBool,
        files: Arc<AtomicU64>,
        bytes: Arc<AtomicU64>,
        skipped: Arc<AtomicU64>,
        errors: Arc<AtomicU64>,
        last_file: Arc<std::sync::Mutex<String>>,
    ) -> anyhow::Result<PullStats> {
        let source = PhysicalDisk::open(vol.drive_number)?;
        let window = Window::new(source, vol.start_byte, vol.length_bytes)?;
        let mut fs = Hfsplus::open(window, 0)?;
        let opts = PullOptions::default();

        let mut on_event = |ev: PullEvent| match ev {
            PullEvent::FinishedFile {
                dst, bytes_written, ..
            } => {
                files.fetch_add(1, Ordering::Relaxed);
                bytes.fetch_add(bytes_written, Ordering::Relaxed);
                if let Ok(mut s) = last_file.lock() {
                    *s = dst.display().to_string();
                }
            }
            PullEvent::SkippedExisting { .. } | PullEvent::SkippedPrivate { .. } => {
                skipped.fetch_add(1, Ordering::Relaxed);
            }
            PullEvent::Error { .. } => {
                errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        };

        pull_tree(&mut fs, src_path, dst_dir, &opts, cancel, &mut on_event)
    }

    fn scan_volumes() -> ScanResult {
        let mut out = Vec::new();
        let disks = physical::enumerate();
        tracing::info!("scan: enumerated {} disk(s)", disks.len());
        for disk in disks {
            match scan_disk(disk.clone()) {
                Ok(vols) => {
                    tracing::info!(
                        "scan: disk {} -> {} Mac volume(s)",
                        disk.drive_number,
                        vols.len()
                    );
                    out.extend(vols);
                }
                Err(e) => {
                    tracing::warn!("scan: disk {} skipped: {:#}", disk.drive_number, e);
                }
            }
        }
        ScanResult::Done(out)
    }

    fn scan_disk(disk: DiskInfo) -> anyhow::Result<Vec<ScannedVolume>> {
        let mut source = PhysicalDisk::open(disk.drive_number)?;
        let parts = partition::probe(&mut source)?;
        let mut out = Vec::new();
        for p in parts {
            if !matches!(p.scheme, PartitionScheme::Gpt | PartitionScheme::Apm) {
                continue;
            }
            if !p.is_mac_filesystem() {
                continue;
            }
            let volume_label = read_volume_label(disk.drive_number, &p).unwrap_or_default();
            out.push(ScannedVolume {
                drive_number: disk.drive_number,
                partition_label: p.name.clone(),
                volume_label,
                start_byte: p.start_byte,
                length_bytes: p.length_bytes,
            });
        }
        Ok(out)
    }

    fn read_volume_label(drive_number: u32, p: &Partition) -> anyhow::Result<String> {
        let source = PhysicalDisk::open(drive_number)?;
        let window = Window::new(source, p.start_byte, p.length_bytes)?;
        let fs = Hfsplus::open(window, 0)?;
        Ok(fs.volume_label().unwrap_or("").to_string())
    }

    fn candidate_letters(in_use: &std::collections::HashSet<String>) -> Vec<String> {
        let mut letters = Vec::new();
        for c in b'D'..=b'Z' {
            let label = format!("{}:", c as char);
            let drive_root = format!("{}:\\", c as char);
            if std::path::Path::new(&drive_root).exists() {
                continue;
            }
            if in_use.contains(&label) {
                continue;
            }
            letters.push(label);
        }
        letters
    }

    fn first_free_letter(in_use: &std::collections::HashSet<String>) -> Option<String> {
        candidate_letters(in_use).into_iter().next_back()
    }

    fn find_launchctl() -> Option<PathBuf> {
        // Prefer the registered InstallDir; fall back to standard locations.
        if let Some(install) = winfsp_install_dir() {
            let p = PathBuf::from(install).join("bin").join("launchctl-x64.exe");
            if p.exists() {
                return Some(p);
            }
        }
        for root in [r"C:\Program Files (x86)\WinFsp", r"C:\Program Files\WinFsp"] {
            let p = PathBuf::from(root).join("bin").join("launchctl-x64.exe");
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    fn winfsp_install_dir() -> Option<String> {
        for root in [r"HKLM\SOFTWARE\WOW6432Node\WinFsp", r"HKLM\SOFTWARE\WinFsp"] {
            let out = Command::new("reg")
                .args(["query", root, "/v", "InstallDir"])
                .output()
                .ok()?;
            if !out.status.success() {
                continue;
            }
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(rest) = line.trim().strip_prefix("InstallDir") {
                    let mut it = rest.trim().splitn(2, char::is_whitespace);
                    let _ty = it.next();
                    if let Some(value) = it.next() {
                        let v = value.trim().to_string();
                        if !v.is_empty() {
                            return Some(v);
                        }
                    }
                }
            }
        }
        None
    }

    fn is_service_registered() -> bool {
        let Ok(out) = Command::new("reg")
            .args(["query", SERVICE_REG, "/v", "Executable", "/reg:32"])
            .output()
        else {
            return false;
        };
        out.status.success()
    }

    fn locate_mount_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        let candidate = dir.join("applesauce-mount.exe");
        if candidate.exists() {
            Some(candidate)
        } else {
            None
        }
    }

    /// Launch `exe` with the given args under a UAC prompt and wait
    /// for it to exit. Implemented via PowerShell's `Start-Process
    /// -Verb RunAs -Wait` to avoid pulling in the giant
    /// `Win32_UI_Shell_Common` feature surface of the `windows` crate.
    fn runas(exe: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
        let exe_str = exe.to_string_lossy().to_string();
        let arg_list = args
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");

        let script = format!(
            "$p = Start-Process -FilePath '{}' -ArgumentList @({}) -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
            exe_str.replace('\'', "''"),
            arg_list
        );

        let status = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()?;

        if !status.success() {
            anyhow::bail!("elevated install failed (exit {status})");
        }
        Ok(())
    }

    fn is_admin() -> bool {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        let mut token = HANDLE::default();
        unsafe {
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut elev = TOKEN_ELEVATION::default();
            let mut ret_len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elev as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut ret_len,
            )
            .is_ok();
            let _ = windows::Win32::Foundation::CloseHandle(token);
            ok && elev.TokenIsElevated != 0
        }
    }
}
