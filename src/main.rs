//! vpngate-gui: VPN Gate GUI for Ubuntu (Rust + eframe/egui)。

mod app;
mod nmcli;
mod store;
mod vpngate;

use app::VpnGuiApp;

fn app_icon() -> std::sync::Arc<egui::IconData> {
    let rgba: &[u8] = include_bytes!("../assets/icon-64.rgba");
    std::sync::Arc::new(egui::IconData {
        rgba: rgba.to_vec(),
        width: 64,
        height: 64,
    })
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_title("VPN Gate GUI")
            .with_app_id("vpngate-gui")
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "vpngate-gui",
        options,
        Box::new(|cc| Ok(Box::new(VpnGuiApp::new(&cc.egui_ctx)))),
    )
}
