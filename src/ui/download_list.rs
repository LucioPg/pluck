use crate::browser::format_bytes;
use crate::download::types::{
    DownloadControl, DownloadId, DownloadItem, DownloadState, format_time,
};
use crate::ui::thumb::Thumb;
use dioxus::prelude::*;
use std::collections::HashMap;

/// Pane unico dei download: alto ~2/3 della pagina, scrollabile, a RIGHE
/// (non card). Badge stato, percentuale e bottoni sempre visibili; il nome
/// file va in ellipsis se troppo lungo (tooltip col path completo).
#[component]
pub fn DownloadList(
    downloads: Signal<Vec<DownloadItem>>,
    controls: Signal<HashMap<DownloadId, DownloadControl>>,
    on_open_failed: EventHandler<String>,
    on_remove: EventHandler<DownloadId>,
    on_clear_all: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "flex-1 w-full rounded-box border-2 border-base-300 bg-base-200 p-3 shadow-sm flex flex-col gap-1.5 overflow-y-auto",
            if !downloads.read().is_empty() {
                div { class: "flex justify-end shrink-0",
                    button {
                        class: "btn btn-xs btn-ghost",
                        title: "Remove all downloads from the list (active ones are cancelled). Files on disk are not touched.",
                        onclick: move |_| on_clear_all.call(()),
                        "Clear list"
                    }
                }
            }
            for item in downloads.read().iter() {
                DownloadRow {
                    key: "{item.id}",
                    item: item.clone(),
                    controls,
                    on_open_failed,
                    on_remove,
                }
            }
            if downloads.read().is_empty() {
                div { class: "m-auto rounded-box border border-dashed border-base-300 px-6 py-4 text-center text-sm opacity-60",
                    "No downloads yet. Use the \"＋ Manual\" button to start one."
                }
            }
        }
    }
}

#[component]
fn DownloadRow(
    item: DownloadItem,
    controls: Signal<HashMap<DownloadId, DownloadControl>>,
    on_open_failed: EventHandler<String>,
    on_remove: EventHandler<DownloadId>,
) -> Element {
    let is_terminal = matches!(
        item.state,
        DownloadState::Completed | DownloadState::Failed(_) | DownloadState::Cancelled
    );
    let indeterminate = item.total == 0 && item.state == DownloadState::Downloading;

    // Doppio-click sulla riga: apre il file (solo Completed; l'handler è
    // collegato a tutte le righe, no-op altrimenti — vedi KB Dioxus 0.7)
    let can_open = item.state == DownloadState::Completed;
    let open_path = item.filename.clone();
    let open_file = move |_| {
        if !can_open {
            return;
        }
        let path = open_path.clone();
        spawn(async move {
            if !path.exists() {
                on_open_failed.call("File not found on disk".into());
                return;
            }
            if let Err(e) = open::that(&path) {
                on_open_failed.call(format!("Failed to open: {e}"));
            }
        });
    };

    let full_path = item.filename.display().to_string();
    let name = item
        .filename
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let badge = match &item.state {
        DownloadState::Completed => rsx! {
            span { class: "badge badge-success shrink-0", "Completed" }
        },
        DownloadState::Failed(reason) => rsx! {
            span {
                class: "badge badge-error shrink-0",
                title: "{reason}",
                "Failed"
            }
        },
        DownloadState::Cancelled => rsx! {
            span { class: "badge badge-warning shrink-0", "Cancelled" }
        },
        DownloadState::Paused => rsx! {
            span { class: "badge badge-ghost shrink-0", "Paused" }
        },
        DownloadState::Downloading => rsx! {
            span { class: "badge badge-primary shrink-0",
                if item.is_hls { "Stream" } else { "Download" }
            }
        },
    };

    rsx! {
        div {
            class: if can_open {
                "cursor-pointer flex items-center gap-2 rounded-lg bg-base-100 px-3 py-0.5 text-xs shadow-sm"
            } else {
                "cursor-default flex items-center gap-2 rounded-lg bg-base-100 px-3 py-0.5 text-xs shadow-sm"
            },
            ondoubleclick: open_file,
            // Thumbnail quadrata del candidato sorgente (placeholder se None)
            Thumb { url: item.thumbnail.clone() }
            {badge}
            // Nome file: ellipsis se lungo, tooltip col path completo
            span {
                class: "truncate min-w-0 flex-1 font-medium",
                title: "{full_path}",
                "{name}"
            }
            // Progress circolare compatto: la % è DENTRO il cerchio
            if indeterminate {
                span {
                    class: "loading loading-spinner loading-primary shrink-0",
                    title: "Unknown size (no Content-Length)",
                }
            } else {
                div {
                    class: "radial-progress text-primary shrink-0",
                    style: "--value:{item.percentage()}; --size:1.9rem; --thickness:3px",
                    "{item.percentage()}%"
                }
            }
            // Dimensioni: sempre visibili. HLS = tempo (current/total sono
            // secondi), engine normale = byte
            span { class: "tabular-nums opacity-70 shrink-0 w-36 text-right",
                if item.is_hls {
                    if item.total > 0 {
                        "{format_time(item.current)} / {format_time(item.total)}"
                    } else if item.current > 0 {
                        "{format_time(item.current)}"
                    } else {
                        "…"
                    }
                } else if item.total > 0 {
                    "{format_bytes(item.current)} / {format_bytes(item.total)}"
                } else {
                    "{format_bytes(item.current)}"
                }
            }
            div { class: "flex gap-1 shrink-0",
                if item.state == DownloadState::Downloading && !item.is_hls {
                    button {
                        class: "btn btn-xs",
                        onclick: move |_| {
                            if let Some(ctl) = controls.read().get(&item.id) {
                                let _ = ctl.pause.send(true);
                            }
                        },
                        "Pause"
                    }
                }
                if item.state == DownloadState::Paused {
                    button {
                        class: "btn btn-xs btn-primary",
                        onclick: move |_| {
                            if let Some(ctl) = controls.read().get(&item.id) {
                                let _ = ctl.pause.send(false);
                            }
                        },
                        "Resume"
                    }
                }
                if !is_terminal {
                    button {
                        class: "btn btn-xs btn-error",
                        onclick: move |_| {
                            if let Some(ctl) = controls.read().get(&item.id) {
                                ctl.cancel.cancel();
                            }
                        },
                        "Cancel"
                    }
                }
                if is_terminal {
                    button {
                        class: "btn btn-xs btn-ghost",
                        title: "Remove from list (file on disk is not touched)",
                        aria_label: "Remove download from list",
                        onclick: move |_| on_remove.call(item.id),
                        "✕"
                    }
                }
            }
        }
    }
}
