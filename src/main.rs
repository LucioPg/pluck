//! pluck — desktop download manager.
//!
//! Senza argomenti avvia la UI grafica; con `pluck <url> [referrer] [on|off]`
//! scarica direttamente da linea di comando (vedi `cli::usage`).

mod browser;
mod cli;
mod download;
mod referers;
mod ui;

use dioxus::desktop::{Config, WindowBuilder, WindowCloseBehaviour};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        Err(e) => {
            eprintln!("Errore: {e}\n\n{}", cli::usage());
            std::process::exit(2);
        }
        Ok(cli::CliAction::Help) => {
            print!("{}", cli::usage());
        }
        Ok(cli::CliAction::Download(cli_args)) => {
            let rt = tokio::runtime::Runtime::new().expect("runtime tokio");
            let code = rt.block_on(cli::download(cli_args));
            std::process::exit(code);
        }
        Ok(cli::CliAction::Ui) => run_ui(),
    }
}

fn run_ui() {
    let icon = load_window_icon();
    let mut builder = WindowBuilder::new()
        .with_title("pluck")
        .with_inner_size(dioxus::desktop::LogicalSize::new(820.0, 600.0))
        .with_resizable(true);
    if let Some(icon) = icon {
        builder = builder.with_window_icon(Some(icon));
    }
    // with_menu(None): senza, dioxus-desktop inietta la menu bar di default
    // (File/Edit/View/Help) anche in release.
    // WindowHides: la X nasconde la finestra (app in systray); l'uscita vera
    // è dal menu dell'icona nella tray ("Esci").
    let config = Config::new()
        .with_window(builder)
        .with_menu(None)
        .with_close_behaviour(WindowCloseBehaviour::WindowHides);
    dioxus::LaunchBuilder::new()
        .with_cfg(config)
        .launch(ui::app::App);
}

/// Icona finestra: decode icon.ico → RGBA → tao Icon (pattern gui_launcher PWDManager).
fn load_window_icon() -> Option<dioxus::desktop::tao::window::Icon> {
    const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.ico");
    let img = image::load_from_memory(ICON_BYTES).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    dioxus::desktop::tao::window::Icon::from_rgba(img.into_raw(), width, height).ok()
}
