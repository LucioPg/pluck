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

    // Chiusure submit: cloni tx per consumer (le props vogliono FnMut)
    let tx_auto = tx.clone();
    let auto_on_download =
        move |(url, referer, dest): (String, Option<String>, PathBuf)| {
            launch_download(
                tx_auto.clone(),
                downloads,
                controls,
                referer_store,
                url,
                referer,
                dest,
            );
        };
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

    // Privacy: alla chiusura della finestra → shutdown browser (rilascio lock
    // profilo) + cancellazione della cartella profilo (cookie/login/cache non
    // persistono su disco). Retry: il processo browser può rilasciare i file
    // qualche centinaio di ms dopo il Close().
    {
        let dx = dioxus::desktop::window();
        let cleanup_done = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = cleanup_done.clone();
        dx.create_wry_event_handler(move |event, _| {
            if !matches!(
                event,
                dioxus::desktop::tao::event::Event::WindowEvent {
                    event: dioxus::desktop::tao::event::WindowEvent::CloseRequested { .. },
                    ..
                }
            ) || flag.get()
            {
                return;
            }
            flag.set(true);
            eprintln!("[exit] chiusura: shutdown browser + cancellazione profilo");
            if let Some(handle) = crate::browser::shared_browser()
                && !handle.shutdown_and_wait(std::time::Duration::from_secs(3))
            {
                eprintln!("[exit] ack shutdown non arrivato (timeout): procedo comunque");
            }
            let profile = crate::browser::profile_dir();
            for attempt in 0..5 {
                match std::fs::remove_dir_all(&profile) {
                    Ok(()) => {
                        eprintln!("[exit] profilo cancellato ({})", profile.display());
                        // Rimuovi anche la cartella padre se rimasta vuota
                        if let Some(parent) = profile.parent() {
                            let _ = std::fs::remove_dir(parent);
                        }
                        return;
                    }
                    Err(e) if attempt < 4 => {
                        eprintln!("[exit] cancellazione ritentata ({e})");
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    Err(e) => {
                        eprintln!("[exit] ERRORE: profilo non cancellato: {e}");
                    }
                }
            }
        });
    }

    rsx! {
        head { style { "{TAILWIND_CSS}" } }
        div { class: "h-screen overflow-hidden bg-base-100 p-6",
            div { class: "max-w-2xl mx-auto h-full flex flex-col gap-4",
                header { class: "flex items-center justify-between gap-4",
                    h1 { class: "text-2xl font-bold shrink-0", "ldwhelper" }
                    button {
                        class: "btn btn-primary",
                        onclick: move |_| dialog_open.set(true),
                        "＋ Manuale"
                    }
                }

                // Sezione automatica: browser in background + candidati (sempre visibile).
                // shrink-0 + max-h: non può mai sbordare sopra il pane download.
                div { class: "shrink-0 max-h-[45vh] overflow-y-auto",
                    crate::ui::auto_page::AutoPage {
                        on_download: auto_on_download,
                        on_error: move |msg: String| {
                            push_toast(toasts, msg);
                        },
                    }
                }

                // Separazione netta tra pannello principale e lista
                div { class: "divider my-0 text-xs uppercase tracking-wide opacity-60",
                    "Download"
                }

                // Pane download: wrapper con minimo 1/4 pagina; l'interno scrolla
                div { class: "flex-1 min-h-[25vh] flex overflow-hidden",
                    crate::ui::download_list::DownloadList {
                        downloads,
                        controls,
                        on_open_failed: move |msg: String| {
                            push_toast(toasts, msg);
                        },
                    }
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
