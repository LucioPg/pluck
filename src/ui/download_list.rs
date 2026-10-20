use crate::download::types::{DownloadControl, DownloadId, DownloadItem, DownloadState};
use dioxus::prelude::*;
use std::collections::HashMap;

#[component]
pub fn DownloadList(
    mut downloads: Signal<Vec<DownloadItem>>,
    controls: Signal<HashMap<DownloadId, DownloadControl>>,
) -> Element {
    rsx! {
        div { class: "flex flex-col gap-3",
            for item in downloads.read().iter() {
                DownloadCard {
                    key: "{item.id}",
                    item: item.clone(),
                    controls,
                }
            }
            if downloads.read().is_empty() {
                p { class: "text-center opacity-60", "Nessun download in corso" }
            }
        }
    }
}

#[component]
fn DownloadCard(
    item: DownloadItem,
    controls: Signal<HashMap<DownloadId, DownloadControl>>,
) -> Element {
    let is_terminal = matches!(
        item.state,
        DownloadState::Completed | DownloadState::Failed(_) | DownloadState::Cancelled
    );
    let indeterminate = item.total == 0 && item.state == DownloadState::Downloading;

    let progress_el = if indeterminate {
        rsx! { progress { class: "progress progress-primary w-full" } }
    } else {
        rsx! {
            progress {
                class: "progress progress-primary w-full",
                value: "{item.current}",
                max: "{item.total.max(1)}",
            }
        }
    };

    rsx! {
        div { class: "card bg-base-200",
            div { class: "card-body gap-2 p-4",
                div { class: "flex items-center justify-between gap-2",
                    span { class: "font-medium truncate", title: "{item.filename.display()}",
                        "{item.filename.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()}"
                    }
                    match &item.state {
                        DownloadState::Completed => rsx! { span { class: "badge badge-success", "Completato" } },
                        DownloadState::Failed(_) => rsx! { span { class: "badge badge-error", "Errore" } },
                        DownloadState::Cancelled => rsx! { span { class: "badge badge-warning", "Annullato" } },
                        DownloadState::Paused => rsx! { span { class: "badge badge-ghost", "In pausa" } },
                        DownloadState::Downloading => rsx! { span { class: "badge badge-primary", "Download" } },
                    }
                }
                {progress_el}
                if let DownloadState::Failed(reason) = &item.state {
                    div { class: "alert alert-error py-2 text-sm", "{reason}" }
                }
                div { class: "flex items-center justify-between text-sm opacity-70",
                    span {
                        if item.total > 0 {
                            "{item.current}/{item.total} byte — {item.percentage()}%"
                        } else {
                            "{item.current} byte"
                        }
                    }
                    div { class: "flex gap-2",
                        if item.state == DownloadState::Downloading {
                            button {
                                class: "btn btn-xs",
                                onclick: move |_| {
                                    if let Some(ctl) = controls.read().get(&item.id) {
                                        let _ = ctl.pause.send(true);
                                    }
                                },
                                "Pausa"
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
                                "Riprendi"
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
                                "Annulla"
                            }
                        }
                    }
                }
            }
        }
    }
}
