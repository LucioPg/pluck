//! ldwhelper — desktop download manager.

mod browser;
mod download;
mod referers;
mod ui;

use dioxus::desktop::{Config, WindowBuilder};

fn main() {
    let icon = load_window_icon();
    let mut builder = WindowBuilder::new()
        .with_title("ldwhelper")
        .with_inner_size(dioxus::desktop::LogicalSize::new(820.0, 600.0))
        .with_resizable(true);
    if let Some(icon) = icon {
        builder = builder.with_window_icon(Some(icon));
    }
    let config = Config::new().with_window(builder);
    dioxus::LaunchBuilder::new().with_cfg(config).launch(ui::app::App);
}

/// Icona finestra: decode icon.ico → RGBA → tao Icon (pattern gui_launcher PWDManager).
fn load_window_icon() -> Option<dioxus::desktop::tao::window::Icon> {
    const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.ico");
    let img = image::load_from_memory(ICON_BYTES).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    dioxus::desktop::tao::window::Icon::from_rgba(img.into_raw(), width, height).ok()
}
