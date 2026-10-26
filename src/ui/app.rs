use crate::download::engine::spawn_download;
use crate::download::hls::spawn_hls_download;
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
#[allow(clippy::too_many_arguments)]
fn launch_download(
    tx: mpsc::Sender<ProgressMessage>,
    mut downloads: Signal<Vec<DownloadItem>>,
    mut controls: Signal<HashMap<DownloadId, DownloadControl>>,
    mut referer_store: Signal<RefererStore>,
    url: String,
    referer: Option<String>,
    dest: PathBuf,
    thumbnail: Option<String>,
    is_hls: bool,
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
        thumbnail,
        is_hls,
    });
    // HLS: ffmpeg (sidecar) scarica i segmenti e remuxa in MP4; engine normale
    // altrimenti (range HTTP con pausa/ripresa)
    let ctl = if is_hls {
        spawn_hls_download(id, url, referer.clone(), dest, tx)
    } else {
        spawn_download(id, url, referer.clone(), dest, tx)
    };
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
    // Spoofing referrer: invio dell'header Referer sui download/probe. Attivo di
    // default (scelta punto 1 todo); il radio nell'header lo disattiva globalmente.
    let mut spoofing = use_signal(|| true);
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
    let auto_on_download = move |(url, referer, dest, thumbnail, is_stream): (
        String,
        Option<String>,
        PathBuf,
        Option<String>,
        bool,
    )| {
        launch_download(
            tx_auto.clone(),
            downloads,
            controls,
            referer_store,
            url,
            referer,
            dest,
            thumbnail,
            is_stream,
        );
    };
    let tx_manual = tx.clone();
    let manual_on_submit = move |(url, referer, dest): (String, Option<String>, PathBuf)| {
        launch_download(
            tx_manual.clone(),
            downloads,
            controls,
            referer_store,
            url,
            referer,
            dest,
            None, // download manuale: nessuna thumbnail
            false,
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
                    h1 { class: "text-2xl font-bold shrink-0", "ldwhelper v{env!(\"CARGO_PKG_VERSION\")}" }
                    div { class: "flex items-center gap-3",
                        // Spoofing referrer: ON/OFF globale (default ON)
                        div {
                            class: "flex items-center gap-2.5 rounded-full bg-base-200 px-3.5 py-1.5 shrink-0",
                            title: "Spoofing referrer: invia l'header Referer con i download (sostituzione del referrer)",
                            span { class: "text-xs font-semibold uppercase tracking-wide opacity-60", "Spoofing" }
                            label { class: "flex cursor-pointer items-center gap-1.5 text-sm",
                                input {
                                    r#type: "radio",
                                    name: "spoofing-referrer",
                                    class: "radio radio-primary radio-xs",
                                    checked: *spoofing.read(),
                                    onclick: move |_| spoofing.set(true),
                                    aria_label: "Spoofing referrer attivo",
                                }
                                "On"
                            }
                            label { class: "flex cursor-pointer items-center gap-1.5 text-sm",
                                input {
                                    r#type: "radio",
                                    name: "spoofing-referrer",
                                    class: "radio radio-primary radio-xs",
                                    checked: !*spoofing.read(),
                                    onclick: move |_| spoofing.set(false),
                                    aria_label: "Spoofing referrer disattivato",
                                }
                                "Off"
                            }
                        }
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| dialog_open.set(true),
                            "＋ Manuale"
                        }
                    }
                }

                // Banner persistente: file referer illeggibile/corrotto → l'utente
                // sa DOVE intervenire; la persistenza resta disabilitata finché
                // non lo corregge (mai sovrascritto automaticamente)
                if let Some(reason) = referer_store.read().corrupted() {
                    div { class: "alert alert-warning text-sm shrink-0",
                        span { class: "shrink-0", "⚠" }
                        span { class: "min-w-0 break-all",
                            "File referer corrotto ({reason}) — salvataggio disabilitato. File: "
                            span { class: "font-mono", "{referer_store.read().path().display()}" }
                        }
                    }
                }

                // Sezione automatica: browser in background + candidati (sempre visibile).
                // shrink-0 + max-h: non può mai sbordare sopra il pane download.
                div { class: "shrink-0 max-h-[45vh] overflow-y-auto",
                    crate::ui::auto_page::AutoPage {
                        spoofing,
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
                    spoofing,
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
