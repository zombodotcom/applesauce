//! applesauce — mount Mac drives as Windows drive letters.
//!
//! The GUI is intentionally tiny: scan, mount, unmount. The user does
//! their actual file work in Windows Explorer (or whichever Windows
//! tool they prefer).

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 480.0])
            .with_title("applesauce"),
        ..Default::default()
    };

    eframe::run_native(
        "applesauce",
        options,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
}

#[derive(Default)]
struct App {
    status: String,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("applesauce");
            ui.label("Mount Mac drives as Windows drive letters.");
            ui.separator();
            ui.label("No drives detected yet — disk scanning lands in the next commit.");
            if !self.status.is_empty() {
                ui.label(&self.status);
            }
        });
    }
}
