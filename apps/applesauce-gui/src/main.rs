//! applesauce — mount Mac drives as Windows drive letters.
//!
//! The GUI is intentionally tiny: scan, mount, unmount. The user does
//! their actual file work in Windows Explorer (or whichever Windows
//! tool they prefer).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 480.0])
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
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};

    use block_source::partition::{self, Partition, PartitionScheme};
    use block_source::physical::{self, DiskInfo, PhysicalDisk};
    use block_source::window::Window;
    use fs_core::hfsplus::Hfsplus;
    use fs_core::MacFilesystem;
    use winfsp_bridge::MountedHost;

    /// A scanned Mac-typed partition we know how to mount.
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

    enum MountResult {
        Mounted { key: MountKey, host: MountedHost },
        Err(String),
    }

    /// Identifies an active mount in the UI list. (drive_number,
    /// start_byte) is unique for the lifetime of a Mac drive.
    #[derive(Clone, PartialEq, Eq, Hash)]
    struct MountKey {
        drive_number: u32,
        start_byte: u64,
        mountpoint: String,
    }

    struct ActiveMount {
        key: MountKey,
        host: MountedHost,
    }

    pub struct App {
        volumes: Vec<ScannedVolume>,
        active: Vec<ActiveMount>,
        scanning: Option<(JoinHandle<()>, Receiver<ScanResult>)>,
        mounting: Option<(JoinHandle<()>, Receiver<MountResult>)>,
        // Per-row drive-letter selection.
        chosen_letter: std::collections::HashMap<(u32, u64), String>,
        status: String,
        admin: bool,
    }

    impl App {
        pub fn new() -> Self {
            let mut me = Self {
                volumes: Vec::new(),
                active: Vec::new(),
                scanning: None,
                mounting: None,
                chosen_letter: Default::default(),
                status: String::new(),
                admin: is_admin(),
            };
            me.start_scan();
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
            let done = self
                .scanning
                .as_ref()
                .and_then(|(_, rx)| rx.try_recv().ok())
                .is_some();
            // Re-borrow to actually consume the message (try_recv above is just a peek).
            if done {
                if let Some((handle, rx)) = self.scanning.take() {
                    let res = rx.recv().ok();
                    let _ = handle.join();
                    match res {
                        Some(ScanResult::Done(v)) => {
                            self.status = format!("Found {} Mac volume(s).", v.len());
                            self.volumes = v;
                        }
                        None => self.status = "Scan thread vanished.".to_string(),
                    }
                }
            }
        }

        fn poll_mount(&mut self) {
            let done = self
                .mounting
                .as_ref()
                .and_then(|(_, rx)| rx.try_recv().ok())
                .is_some();
            if done {
                if let Some((handle, rx)) = self.mounting.take() {
                    let res = rx.recv().ok();
                    let _ = handle.join();
                    match res {
                        Some(MountResult::Mounted { key, host }) => {
                            self.status = format!("Mounted on {}.", key.mountpoint);
                            self.active.push(ActiveMount { key, host });
                        }
                        Some(MountResult::Err(e)) => {
                            self.status = format!("Mount failed: {e}");
                        }
                        None => self.status = "Mount thread vanished.".to_string(),
                    }
                }
            }
        }

        fn start_mount(&mut self, vol_idx: usize, letter: String) {
            let v = match self.volumes.get(vol_idx) {
                Some(v) => v,
                None => return,
            };
            let key = MountKey {
                drive_number: v.drive_number,
                start_byte: v.start_byte,
                mountpoint: letter,
            };
            let drive_number = v.drive_number;
            let start = v.start_byte;
            let length = v.length_bytes;
            let mountpoint = key.mountpoint.clone();
            let key_for_thread = key.clone();
            let (tx, rx) = mpsc::channel::<MountResult>();
            let handle = thread::spawn(move || {
                let res = mount_partition(drive_number, start, length, &mountpoint)
                    .map(|host| MountResult::Mounted {
                        key: key_for_thread,
                        host,
                    })
                    .unwrap_or_else(|e| MountResult::Err(format!("{e:#}")));
                let _ = tx.send(res);
            });
            self.mounting = Some((handle, rx));
            self.status = format!("Mounting {}…", key.mountpoint);
        }

        fn unmount(&mut self, idx: usize) {
            if idx >= self.active.len() {
                return;
            }
            let m = self.active.swap_remove(idx);
            let mp = m.key.mountpoint.clone();
            // Drop on a background thread — unmount can block briefly
            // while WinFsp tears down its kernel-side state.
            thread::spawn(move || {
                m.host.unmount();
            });
            self.status = format!("Unmounted {mp}.");
        }
    }

    impl eframe::App for App {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            // Poll background jobs each frame.
            self.poll_scan();
            self.poll_mount();
            if self.scanning.is_some() || self.mounting.is_some() {
                ctx.request_repaint();
            }

            egui::TopBottomPanel::top("top").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("applesauce");
                    ui.label("— mount Mac drives as Windows drive letters");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(self.scanning.is_none(), egui::Button::new("Rescan"))
                            .clicked()
                        {
                            self.start_scan();
                        }
                    });
                });
            });

            egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
                if !self.admin {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 120, 0),
                        "Not running as Administrator — physical disks won't appear. \
                         Right-click → Run as administrator.",
                    );
                }
                ui.label(&self.status);
            });

            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("Mac volumes");
                if self.volumes.is_empty() && self.scanning.is_none() {
                    ui.label("No HFS+ volumes detected. Plug in a Mac drive, then click Rescan.");
                }

                let busy_mounting = self.mounting.is_some();
                let active_letters: std::collections::HashSet<String> = self
                    .active
                    .iter()
                    .map(|m| m.key.mountpoint.clone())
                    .collect();

                let mut mount_request: Option<(usize, String)> = None;

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
                            let already = self.active.iter().any(|m| {
                                m.key.drive_number == v.drive_number
                                    && m.key.start_byte == v.start_byte
                            });

                            if already {
                                ui.add_enabled(false, egui::Button::new("Mounted"));
                            } else {
                                let chosen = self.chosen_letter.entry(key).or_insert_with(|| {
                                    first_free_letter(&active_letters)
                                        .unwrap_or_else(|| "Z:".to_string())
                                });

                                if ui
                                    .add_enabled(
                                        !busy_mounting && self.admin,
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
                            ui.label(format!(
                                "{}  ←  disk {} @ {} GiB offset",
                                m.key.mountpoint,
                                m.key.drive_number,
                                m.key.start_byte as f64 / (1u64 << 30) as f64
                            ));
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
            });
        }
    }

    fn scan_volumes() -> ScanResult {
        let mut out = Vec::new();
        for disk in physical::enumerate() {
            match scan_disk(disk) {
                Ok(mut vols) => out.append(&mut vols),
                Err(_) => continue, // skip disks that we can't probe
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

    fn mount_partition(
        drive_number: u32,
        start_byte: u64,
        length_bytes: u64,
        mountpoint: &str,
    ) -> anyhow::Result<MountedHost> {
        let source = PhysicalDisk::open(drive_number)?;
        let window = Window::new(source, start_byte, length_bytes)?;
        let fs = Hfsplus::open(window, 0)?;
        winfsp_bridge::mount(fs, length_bytes, mountpoint)
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
