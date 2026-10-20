use dioxus::prelude::*;
use std::path::PathBuf;

#[component]
pub fn NewDownloadDialog(
    mut open: Signal<bool>,
    /// Referer preferiti salvati (per il dropdown)
    referers: Vec<String>,
    /// (url, referer, destinazione) — chiamato solo se validazione OK
    on_submit: EventHandler<(String, Option<String>, PathBuf)>,
    /// riservato: rfd save_file non espone errori (None = annullato); hook per toast futuri
    on_error: EventHandler<String>,
) -> Element {
    let _ = open;
    let _ = referers;
    let _ = on_submit;
    let _ = on_error;
    rsx! {}
}
