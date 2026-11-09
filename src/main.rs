//! vpngate-gui: VPN Gate GUI for Ubuntu (Rust + eframe/egui)。

mod app;
mod nmcli;
mod store;
mod vpngate;

use app::VpnGuiApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_title("VPN Gate GUI"),
        ..Default::default()
    };
    eframe::run_native(
        "vpngate-gui",
        options,
        Box::new(|cc| Ok(Box::new(VpnGuiApp::new(&cc.egui_ctx)))),
    )
}
