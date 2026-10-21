use crate::download::engine::spawn_download;
use crate::download::types::*;
use crate::referers::RefererStore;
use dioxus::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// CSS embeddato nel binario (include_str, pattern PWDManager): distribuzione a file singolo.
/// Le modifiche al CSS richiedono ricompilazione (in dev dx serve la gestisce da sé).
const TAILWIND_CSS: &str = include_str!("../../assets/tailwind.css");

fn push_toast(mut toasts: Signal<Vec<String>>, msg: String) {
    toasts.write().push(msg.clone());
    spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        toasts.write().retain(|t| t != &msg);
    });
}

/// Avvia il download e registra il referer nei preferiti (dedup + persist).
/// Funzione pura su parametri Copy: chiamabile da entrambe le tab senza
/// contorsioni di closure (il motore resta in `download/engine.rs`).
fn launch_download(
    tx: mpsc::Sender<ProgressMessage>,
    mut downloads: Signal<Vec<DownloadItem>>,
    mut controls: Signal<HashMap<DownloadId, DownloadControl>>,
    mut referer_store: Signal<RefererStore>,
    url: String,
    referer: Option<String>,
    dest: PathBuf,
) {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    // Push PRIMA dello spawn: il task può inviare messaggi subito — se l'item
    // non è ancora in lista il receiver li scarterebbe (card bloccata su "Download").
    downloads.write().push(DownloadItem {
        id,
        url: url.clone(),
        referer: referer.clone(),
        filename: dest.clone(),
        state: DownloadState::Downloading,
        current: 0,
        total: 0,
    });
    let ctl = spawn_download(id, url, referer.clone(), dest, tx);
    // Referer preferiti: dedup + move-to-front, poi persistenza JSON.
    // Un errore di salvataggio NON blocca il download (lista resta in memoria).
    if let Some(r) = &referer {
        referer_store.write().record(r);
        let _ = referer_store.read().save();
    }
    controls.write().insert(id, ctl);
}

#[component]
pub fn App() -> Element {

    let mut downloads = use_signal(Vec::<DownloadItem>::new);
    let mut controls = use_signal(HashMap::<DownloadId, DownloadControl>::new);
    let mut dialog_open = use_signal(|| false);
    let toasts = use_signal(Vec::<String>::new);
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

    // Chiusura submit manuale: cloni tx per consumer (le props vogliono FnMut)
    let tx_manual = tx.clone();
    let manual_on_submit =
        move |(url, referer, dest): (String, Option<String>, PathBuf)| {
            launch_download(
                tx_manual.clone(),
                downloads,
                controls,
                referer_store,
                url,
                referer,
                dest,
            );
            dialog_open.set(false);
        };

    rsx! {
        head { style { "{TAILWIND_CSS}" } }
        div { class: "min-h-screen bg-base-100 p-6",
            div { class: "max-w-2xl mx-auto flex flex-col gap-4",
                header { class: "flex items-center justify-between gap-4",
                    h1 { class: "text-2xl font-bold shrink-0", "ldwhelper" }
                    button {
                        class: "btn btn-primary",
                        onclick: move |_| dialog_open.set(true),
                        "＋ Manuale"
                    }
                }

                // Pane download: elenco unico, alto ~2/3 pagina, scrollabile
                crate::ui::download_list::DownloadList {
                    downloads,
                    controls,
                    on_open_failed: move |msg: String| {
                        push_toast(toasts, msg);
                    },
                }
            }

            if *dialog_open.read() {
                crate::ui::new_download_dialog::NewDownloadDialog {
                    open: dialog_open,
                    referers: referer_store.read().list().to_vec(),
                    on_submit: manual_on_submit,
                    on_error: move |msg: String| {
                        push_toast(toasts, msg);
                    },
                    on_save_referer: move |r: String| {
                        if r.is_empty() {
                            return;
                        }
                        referer_store.write().record(&r);
                        let _ = referer_store.read().save();
                    },
                    on_remove_referer: move |r: String| {
                        referer_store.write().remove(&r);
                        let _ = referer_store.read().save();
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
