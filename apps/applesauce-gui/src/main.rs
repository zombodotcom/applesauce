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
    use std::collections::{BTreeSet, HashMap, HashSet};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    use block_source::partition::{self, Partition, PartitionScheme};
    use block_source::physical::{self, DiskInfo, PhysicalDisk};
    use block_source::window::Window;
    use fs_core::hfsplus::Hfsplus;
    use fs_core::pull::{pull_tree, sanitize_name, PullEvent, PullOptions, PullStats};
    use fs_core::{DirEntry, MacFilesystem};

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

    /// Lazy-loaded contents of one HFS+ directory.
    enum DirCache {
        Loading,
        Loaded(Vec<DirEntry>),
        Failed(String),
    }

    /// Browse-mode state for a single volume. Each expanded directory's
    /// children are fetched in a background thread and cached here.
    struct BrowseState {
        volume: ScannedVolume,
        cache: HashMap<String, DirCache>,
        expanded: HashSet<String>,
        /// POSIX paths the user has checked as pull roots.
        selected: BTreeSet<String>,
        /// Completed list_dir results land here.
        rx: Receiver<(String, anyhow::Result<Vec<DirEntry>>)>,
        tx: Sender<(String, anyhow::Result<Vec<DirEntry>>)>,
    }

    /// Top-level UI mode. Browse state is boxed because it's much
    /// larger than the unit `Volumes` arm; without the indirection
    /// every `AppView` discriminator pays for the bigger arm.
    enum AppView {
        Volumes,
        Browsing(Box<BrowseState>),
    }

    pub struct App {
        view: AppView,
        volumes: Vec<ScannedVolume>,
        active: Vec<ActiveMount>,
        scanning: Option<(JoinHandle<()>, Receiver<ScanResult>)>,
        mounting: Option<(JoinHandle<()>, Receiver<LaunchResult>)>,
        pulling: Option<PullProgress>,
        // Per-row drive-letter selection.
        chosen_letter: HashMap<(u32, u64), String>,
        // Per-row source path for inline (single-path) Pull.
        pull_src: HashMap<(u32, u64), String>,
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
                view: AppView::Volumes,
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
            let v = match self.volumes.get(vol_idx) {
                Some(v) => v.clone(),
                None => return,
            };
            self.start_pull_roots(&v, vec![(src_path.clone(), dst_dir.clone())], dst_dir);
            self.status = format!("Pulling {src_path}…");
        }

        /// Multi-root variant. `roots` is a list of `(src_path,
        /// dst_parent)` pairs — `pull_tree` will append the sanitized
        /// leaf of `src_path` underneath `dst_parent` for each. `dst_root`
        /// is the user-facing "destination" shown in the status panel
        /// (typically the rfd-picked top-level dir).
        fn start_pull_roots(
            &mut self,
            v: &ScannedVolume,
            roots: Vec<(String, PathBuf)>,
            dst_root: PathBuf,
        ) {
            if self.pulling.is_some() {
                self.status = "Another pull is already running.".to_string();
                return;
            }
            if roots.is_empty() {
                self.status = "Nothing selected to pull.".to_string();
                return;
            }

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
            let v_w = v.clone();

            let handle = thread::spawn(move || {
                let result = run_pulls(
                    &v_w,
                    &roots,
                    &cancel_w,
                    files_w,
                    bytes_w,
                    skipped_w,
                    errors_w,
                    last_file_w,
                );
                let _ = tx.send(PullDone { result });
            });

            self.pulling = Some(PullProgress {
                files,
                bytes,
                skipped,
                errors,
                last_file,
                cancel,
                handle: Some(handle),
                rx,
                dst_root,
                started_at: std::time::Instant::now(),
            });
        }

        /// Translate a list of selected POSIX paths into
        /// `(src_path, dst_parent)` pairs that preserve the volume's
        /// hierarchy under `dst_root`. So `/Users/dave/Documents` lands
        /// at `<dst_root>/Users/dave/Documents` because `pull_tree`
        /// appends `sanitize("Documents")` to `dst_parent`.
        fn roots_for_selection(
            selected: &BTreeSet<String>,
            dst_root: &std::path::Path,
        ) -> Vec<(String, PathBuf)> {
            selected
                .iter()
                .map(|p| {
                    // Drop the leaf — pull_tree adds it back as sanitize(leaf).
                    let components: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
                    let parent_components = if components.is_empty() {
                        Vec::new()
                    } else {
                        components[..components.len() - 1].to_vec()
                    };
                    let mut dst_parent = dst_root.to_path_buf();
                    for c in &parent_components {
                        dst_parent.push(sanitize_name(c));
                    }
                    (p.clone(), dst_parent)
                })
                .collect()
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

        /// Enter browse mode for `vol_idx`. Triggers a background load
        /// of "/" so the tree has something to show immediately.
        fn enter_browse(&mut self, vol_idx: usize) {
            let Some(v) = self.volumes.get(vol_idx) else {
                return;
            };
            let (tx, rx) = mpsc::channel();
            let mut state = BrowseState {
                volume: v.clone(),
                cache: HashMap::new(),
                expanded: HashSet::new(),
                selected: BTreeSet::new(),
                rx,
                tx,
            };
            state.expanded.insert("/".to_string());
            spawn_list_dir(&state, "/".to_string());
            self.view = AppView::Browsing(Box::new(state));
            self.status = format!("Browsing {}…", v.volume_label);
        }

        fn exit_browse(&mut self) {
            self.view = AppView::Volumes;
        }

        fn poll_browse(&mut self) {
            let AppView::Browsing(state) = &mut self.view else {
                return;
            };
            while let Ok((path, result)) = state.rx.try_recv() {
                match result {
                    Ok(entries) => {
                        state.cache.insert(path, DirCache::Loaded(entries));
                    }
                    Err(e) => {
                        state.cache.insert(path, DirCache::Failed(format!("{e:#}")));
                    }
                }
            }
        }
    }

    /// Spawn a background thread that opens the disk, runs `list_dir`,
    /// and sends the result back on `state.tx`.
    fn spawn_list_dir(state: &BrowseState, path: String) {
        let tx = state.tx.clone();
        let vol = state.volume.clone();
        let path_for_thread = path.clone();
        thread::spawn(move || {
            let result = list_dir_once(&vol, &path_for_thread);
            let _ = tx.send((path_for_thread, result));
        });
    }

    impl App {
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
            self.poll_browse();
            if self.scanning.is_some() || self.mounting.is_some() || self.pulling.is_some() {
                ctx.request_repaint();
            }
            if matches!(self.view, AppView::Browsing(_)) {
                // Cheap: repaint while browse worker threads might land
                // new directory listings.
                ctx.request_repaint_after(std::time::Duration::from_millis(150));
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
                let mut mount_request: Option<(usize, String)> = None;
                let mut pull_request: Option<(usize, String)> = None;
                let mut enter_browse_req: Option<usize> = None;
                let mut unmount_req: Option<usize> = None;
                let mut exit_browse_req = false;
                let mut pull_selected_req: Option<(ScannedVolume, Vec<String>)> = None;

                match &mut self.view {
                    AppView::Volumes => {
                        render_volumes_view(
                            ui,
                            &self.volumes,
                            &self.active,
                            self.mounting.is_some(),
                            self.pulling.is_some(),
                            self.admin,
                            self.service_installed,
                            self.launchctl.is_some(),
                            &mut self.chosen_letter,
                            &mut self.pull_src,
                            self.scanning.is_none(),
                            &mut mount_request,
                            &mut pull_request,
                            &mut enter_browse_req,
                            &mut unmount_req,
                        );
                    }
                    AppView::Browsing(state) => {
                        render_browse_view(
                            ui,
                            state,
                            self.pulling.is_some(),
                            &mut exit_browse_req,
                            &mut pull_selected_req,
                        );
                    }
                }

                if let Some(idx) = unmount_req {
                    self.unmount(idx);
                }

                // Apply deferred actions (mutating self after the borrow above).
                if let Some(idx) = enter_browse_req {
                    self.enter_browse(idx);
                }
                if exit_browse_req {
                    self.exit_browse();
                }
                if let Some((vol, paths)) = pull_selected_req {
                    if let Some(dst) = rfd::FileDialog::new()
                        .set_title("Pull destination folder")
                        .pick_folder()
                    {
                        let selected: BTreeSet<String> = paths.into_iter().collect();
                        let roots = App::roots_for_selection(&selected, &dst);
                        self.start_pull_roots(&vol, roots, dst.clone());
                        self.status = format!("Pulling {} root(s)…", selected.len());
                    }
                }
                if let Some((idx, src)) = pull_request {
                    if let Some(dst) = rfd::FileDialog::new()
                        .set_title("Pull destination folder")
                        .pick_folder()
                    {
                        self.start_pull(idx, src, dst);
                    }
                }
                if let Some((idx, letter)) = mount_request {
                    self.start_mount(idx, letter);
                }

                // In-progress pull panel — shown in both views.
                if let Some(p) = &self.pulling {
                    ctx.request_repaint();
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
                    if ui.button("Cancel").clicked() {
                        self.cancel_pull();
                    }
                }
            });
        }
    }

    /// Render the default volume-list view. Side-effects (mount /
    /// pull / browse requests) get pushed into the `*_req` Options for
    /// the caller to act on once the &mut borrow of `App.view` ends.
    #[allow(clippy::too_many_arguments)]
    fn render_volumes_view(
        ui: &mut egui::Ui,
        volumes: &[ScannedVolume],
        active: &[ActiveMount],
        busy_mounting: bool,
        busy_pulling: bool,
        admin: bool,
        service_installed: bool,
        launchctl_present: bool,
        chosen_letter: &mut HashMap<(u32, u64), String>,
        pull_src: &mut HashMap<(u32, u64), String>,
        scan_idle: bool,
        mount_request: &mut Option<(usize, String)>,
        pull_request: &mut Option<(usize, String)>,
        enter_browse_req: &mut Option<usize>,
        unmount_req: &mut Option<usize>,
    ) {
        ui.heading("Mac volumes");
        if volumes.is_empty() && scan_idle && admin {
            ui.label("No HFS+ volumes detected. Plug in a Mac drive, then click Rescan.");
        }

        let active_letters: HashSet<String> = active.iter().map(|m| m.mountpoint.clone()).collect();

        for (i, v) in volumes.iter().enumerate() {
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
                    let already_at = active
                        .iter()
                        .find(|m| m.instance.starts_with(&format!("disk{}-", v.drive_number)));

                    // Browse (opens dedicated tree view).
                    if ui
                        .add_enabled(!busy_pulling && admin, egui::Button::new("Browse…"))
                        .on_hover_text(
                            "Open a tree view of the volume. Check folders to pull, then \
                             click Pull selected.",
                        )
                        .clicked()
                    {
                        *enter_browse_req = Some(i);
                    }

                    ui.separator();

                    // Quick single-path Pull controls.
                    let src = pull_src.entry(key).or_insert_with(|| "/Users".to_string());
                    if ui
                        .add_enabled(!busy_pulling && admin, egui::Button::new("Pull…"))
                        .on_hover_text(
                            "Recursively copy the source path off this volume into \
                             a destination folder you pick. Restartable.",
                        )
                        .clicked()
                    {
                        *pull_request = Some((i, src.clone()));
                    }
                    ui.add(
                        egui::TextEdit::singleline(src)
                            .desired_width(140.0)
                            .hint_text("source path"),
                    );

                    ui.separator();

                    // Mount controls.
                    if let Some(m) = already_at {
                        ui.add_enabled(
                            false,
                            egui::Button::new(format!("Mounted ({})", m.mountpoint)),
                        );
                    } else {
                        let chosen = chosen_letter.entry(key).or_insert_with(|| {
                            first_free_letter(&active_letters).unwrap_or_else(|| "Z:".to_string())
                        });
                        if ui
                            .add_enabled(
                                !busy_mounting && service_installed && launchctl_present,
                                egui::Button::new("Mount"),
                            )
                            .clicked()
                        {
                            *mount_request = Some((i, chosen.clone()));
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

        if !active.is_empty() {
            ui.add_space(12.0);
            ui.heading("Active mounts");
            for (i, m) in active.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("{}  ({})", m.mountpoint, m.instance));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Unmount").clicked() {
                            *unmount_req = Some(i);
                        }
                    });
                });
            }
        }
    }

    /// Render the in-volume tree browser. Selection events get pushed
    /// out via `exit_browse_req` / `pull_selected_req`.
    fn render_browse_view(
        ui: &mut egui::Ui,
        state: &mut BrowseState,
        busy_pulling: bool,
        exit_browse_req: &mut bool,
        pull_selected_req: &mut Option<(ScannedVolume, Vec<String>)>,
    ) {
        ui.horizontal(|ui| {
            if ui.button("◀ Back").clicked() {
                *exit_browse_req = true;
            }
            ui.heading(if state.volume.volume_label.is_empty() {
                state.volume.partition_label.clone()
            } else {
                state.volume.volume_label.clone()
            });
            ui.small(format!(
                "(disk {} · {:.2} GiB)",
                state.volume.drive_number,
                state.volume.length_bytes as f64 / (1u64 << 30) as f64
            ));
        });
        ui.separator();
        ui.small(format!("{} selected", state.selected.len()));

        // Pull-selected button is right-aligned just above the tree.
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy_pulling && !state.selected.is_empty(),
                    egui::Button::new(format!("Pull selected ({})", state.selected.len())),
                )
                .clicked()
            {
                let paths: Vec<String> = state.selected.iter().cloned().collect();
                *pull_selected_req = Some((state.volume.clone(), paths));
            }
            if ui.button("Clear selection").clicked() {
                state.selected.clear();
            }
        });
        ui.separator();

        // Render the tree starting at "/". Pending expansions are
        // queued and applied after the borrow of `state` ends below.
        let mut to_toggle: Vec<String> = Vec::new();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_dir(ui, state, "/", 0, &mut to_toggle);
            });
        for p in to_toggle {
            if state.expanded.contains(&p) {
                state.expanded.remove(&p);
            } else {
                state.expanded.insert(p.clone());
                if !state.cache.contains_key(&p) {
                    state.cache.insert(p.clone(), DirCache::Loading);
                    spawn_list_dir(state, p);
                }
            }
        }
    }

    /// Recursive tree renderer. Each row is:
    ///   [indent] [checkbox] [▶ / ▼ / · for files] [name] [size for files]
    /// File rows are dimmed and have no expand arrow.
    fn render_dir(
        ui: &mut egui::Ui,
        state: &mut BrowseState,
        path: &str,
        depth: usize,
        to_toggle: &mut Vec<String>,
    ) {
        // Snapshot what we need from the cache so we can mutate
        // `state.selected` without borrowing twice.
        let entries: Vec<DirEntry> = match state.cache.get(path) {
            Some(DirCache::Loaded(v)) => v.clone(),
            Some(DirCache::Loading) => {
                ui.horizontal(|ui| {
                    ui.add_space((depth as f32) * 16.0);
                    ui.small(format!("  loading {path}…"));
                });
                return;
            }
            Some(DirCache::Failed(e)) => {
                ui.horizontal(|ui| {
                    ui.add_space((depth as f32) * 16.0);
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), format!("  {e}"));
                });
                return;
            }
            None => return,
        };

        for e in &entries {
            let child_path = if path == "/" {
                format!("/{}", e.name)
            } else {
                format!("{}/{}", path, e.name)
            };
            ui.horizontal(|ui| {
                ui.add_space((depth as f32) * 16.0);

                // Checkbox: drives the selection set.
                let mut checked = state.selected.contains(&child_path);
                if ui.checkbox(&mut checked, "").changed() {
                    if checked {
                        state.selected.insert(child_path.clone());
                    } else {
                        state.selected.remove(&child_path);
                    }
                }

                // Expand toggle (or a non-toggle dot for files).
                if e.is_dir {
                    let icon = if state.expanded.contains(&child_path) {
                        "▼"
                    } else {
                        "▶"
                    };
                    if ui.small_button(icon).clicked() {
                        to_toggle.push(child_path.clone());
                    }
                } else {
                    ui.label("·");
                }

                let label = if e.is_dir {
                    egui::RichText::new(&e.name).strong()
                } else {
                    egui::RichText::new(&e.name)
                };
                ui.label(label);

                if !e.is_dir {
                    ui.weak(human_bytes(e.size_bytes));
                }
            });

            if e.is_dir && state.expanded.contains(&child_path) {
                render_dir(ui, state, &child_path, depth + 1, to_toggle);
            }
        }
    }

    fn human_bytes(b: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut v = b as f64;
        let mut u = 0;
        while v >= 1024.0 && u + 1 < UNITS.len() {
            v /= 1024.0;
            u += 1;
        }
        if u == 0 {
            format!("{b} {}", UNITS[u])
        } else {
            format!("{v:.1} {}", UNITS[u])
        }
    }

    /// Open `vol`'s underlying disk once and run [`pull_tree`] for
    /// each `(src_path, dst_parent)` root sequentially. Returns the
    /// summed [`PullStats`] across all roots. Aborts early if `cancel`
    /// flips. Atomic counters tick every progress event for the UI.
    #[allow(clippy::too_many_arguments)]
    fn run_pulls(
        vol: &ScannedVolume,
        roots: &[(String, PathBuf)],
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

        let mut total = PullStats::default();
        for (src_path, dst_parent) in roots {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            std::fs::create_dir_all(dst_parent)?;

            let files = files.clone();
            let bytes = bytes.clone();
            let skipped = skipped.clone();
            let errors = errors.clone();
            let last_file = last_file.clone();
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

            let stats = pull_tree(&mut fs, src_path, dst_parent, &opts, cancel, &mut on_event)?;
            total.files += stats.files;
            total.dirs += stats.dirs;
            total.bytes += stats.bytes;
            total.skipped += stats.skipped;
            total.errors += stats.errors;
        }
        Ok(total)
    }

    /// Open the disk + volume just long enough to list one directory.
    /// Returns the entries with HFS+ housekeeping entries filtered out
    /// — the browse UI doesn't want to surface `.Spotlight-V100` /
    /// `.fseventsd` / private-data folders.
    fn list_dir_once(vol: &ScannedVolume, path: &str) -> anyhow::Result<Vec<DirEntry>> {
        let source = PhysicalDisk::open(vol.drive_number)?;
        let window = Window::new(source, vol.start_byte, vol.length_bytes)?;
        let mut fs = Hfsplus::open(window, 0)?;
        let mut entries = fs.list_dir(path)?;
        entries.retain(|e| !is_hidden_in_browser(&e.name));
        Ok(entries)
    }

    /// HFS+ / Mac OS X housekeeping entries that should never appear
    /// in the user-facing browse tree. Pulling them is also a no-op
    /// since `fs_core::pull::is_private` skips most of these during
    /// copy, but hiding them from the tree avoids confusion.
    fn is_hidden_in_browser(name: &str) -> bool {
        matches!(
            name,
            ".HFS+ Private Directory Data\u{0d}"
                | "\u{0}\u{0}\u{0}\u{0}HFS+ Private Data"
                | ".HFS+ Private Directory Data"
                | "    HFS+ Private Data"
                | ".Spotlight-V100"
                | ".fseventsd"
                | ".Trashes"
                | ".DocumentRevisions-V100"
                | ".IABootFiles"
                | ".hotfiles.btree"
                | ".journal"
                | ".journal_info_block"
                | ".dbfseventsd"
                | ".vol"
        )
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
