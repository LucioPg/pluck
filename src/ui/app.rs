use crate::download::engine::spawn_download;
use crate::download::types::*;
use crate::referers::RefererStore;
use dioxus::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[component]
pub fn App() -> Element {
    const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");

    let mut downloads = use_signal(Vec::<DownloadItem>::new);
    let mut controls = use_signal(HashMap::<DownloadId, DownloadControl>::new);
    let mut dialog_open = use_signal(|| false);
    let mut toasts = use_signal(Vec::<String>::new);
    // Referer preferiti: caricati una sola volta all'avvio da %APPDATA%/ldwhelper/referers.json
    let mut referer_store = use_signal(|| RefererStore::load(RefererStore::default_path()));

    // Canale creato una sola volta; receiver spawnato qui dentro (niente doppi spawn)
    let tx = use_hook(|| {
        let (tx, mut rx) = mpsc::channel::<ProgressMessage>(100);
        spawn(async move {
            while let Some(msg) = rx.recv().await {
                let mut list = downloads.write();
                if let Some(item) = list.iter_mut().find(|d| d.id == msg.id) {
                    if let Some(state) = msg.state {
                        let terminal = matches!(
                            state,
                            DownloadState::Completed
                                | DownloadState::Failed(_)
                                | DownloadState::Cancelled
                        );
                        item.state = state;
                        // Stato terminale: il control handle non serve più a nessun comando.
                        if terminal {
                            controls.write().remove(&msg.id);
                        }
                    }
                    if msg.total > 0 {
                        item.total = msg.total;
                    }
                    item.current = msg.current;
                }
            }
        });
        tx
    });

    let mut start_download = move |url: String, referer: Option<String>, dest: PathBuf| {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let ctl = spawn_download(id, url.clone(), referer.clone(), dest.clone(), tx.clone());
        // Referer preferiti: dedup + move-to-front, poi persistenza JSON.
        // Un errore di salvataggio NON blocca il download (lista resta in memoria).
        if let Some(r) = &referer {
            referer_store.write().record(r);
            let _ = referer_store.read().save();
        }
        downloads.write().push(DownloadItem {
            id,
            url,
            referer,
            filename: dest,
            state: DownloadState::Downloading,
            current: 0,
            total: 0,
        });
        controls.write().insert(id, ctl);
    };

    rsx! {
        head { link { rel: "stylesheet", href: TAILWIND_CSS } }
        div { class: "min-h-screen bg-base-100 p-6",
            div { class: "max-w-2xl mx-auto flex flex-col gap-4",
                header { class: "flex items-center justify-between",
                    h1 { class: "text-2xl font-bold", "ldwhelper" }
                    button {
                        class: "btn btn-primary",
                        onclick: move |_| dialog_open.set(true),
                        "Nuovo download"
                    }
                }
                crate::ui::download_list::DownloadList { downloads, controls }
            }

            if *dialog_open.read() {
                crate::ui::new_download_dialog::NewDownloadDialog {
                    open: dialog_open,
                    referers: referer_store.read().list().to_vec(),
                    on_submit: move |(url, referer, dest)| {
                        start_download(url, referer, dest);
                        dialog_open.set(false);
                    },
                    on_error: move |msg: String| {
                        toasts.write().push(msg.clone());
                        spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                            toasts.write().retain(|t| t != &msg);
                        });
                    },
                }
            }

            div { class: "toast toast-top toast-center z-50",
                for msg in toasts.read().iter() {
                    div { class: "alert alert-error", span { "{msg}" } }
                }
            }
        }
    }
}
