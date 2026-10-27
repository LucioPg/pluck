use crate::download::engine::spawn_download;
use crate::download::hls::spawn_hls_download;
use crate::download::types::*;
use crate::referers::RefererStore;
use dioxus::desktop::use_muda_event_handler;
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
    bytestream: Option<crate::browser::sniff::BytestreamSource>,
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
    // HLS: ffmpeg (sidecar) scarica i segmenti e remuxa in MP4; bytestream
    // Instagram: video+audio separati scaricati e muxati con ffmpeg; engine
    // normale altrimenti (range HTTP con pausa/ripresa)
    let ctl = if is_hls {
        spawn_hls_download(id, url, referer.clone(), dest, tx)
    } else if let Some(source) = &bytestream {
        crate::download::bytestream::spawn_bytestream_mux_download(id, source.clone(), dest, tx)
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
    // Referer preferiti: caricati una sola volta all'avvio da %APPDATA%/pluck/referers.json
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
    let auto_on_download = move |(url, referer, dest, thumbnail, is_stream, bytestream): (
        String,
        Option<String>,
        PathBuf,
        Option<String>,
        bool,
        Option<crate::browser::sniff::BytestreamSource>,
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
            bytestream,
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
            None, // download manuale: nessuna traccia audio separata
        );
        dialog_open.set(false);
    };

    // System tray: icona + menu (Apri / Esci). Init una tantum sul main thread
    // (pattern PWDManager). La X nasconde la finestra (WindowCloseBehaviour::
    // WindowHides in main.rs): l'uscita vera è solo dal menu.
    use_hook(|| {
        use dioxus::desktop::trayicon;
        let menu = trayicon::menu::Menu::new();
        menu.append(&trayicon::menu::MenuItem::with_id(
            "open",
            "Open pluck",
            true,
            None,
        ))
        .unwrap();
        menu.append(&trayicon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&trayicon::menu::MenuItem::with_id(
            "quit", "Exit", true, None,
        ))
        .unwrap();
        let img =
            image::load_from_memory(include_bytes!("../../assets/icon.ico")).expect("tray icon");
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let icon = trayicon::Icon::from_rgba(rgba.into_raw(), w, h).expect("tray icon rgba");
        trayicon::init_tray_icon(menu, Some(icon));
    });

    // Eventi menu tray: use_muda_event_handler è l'unico dispatch funzionante
    // (dioxus consuma la OnceCell di muda::MenuEvent durante l'init — KB dioxus)
    use_muda_event_handler(move |event| {
        if event.id() == "open" {
            let dx = dioxus::desktop::window();
            dx.window.set_visible(true);
            dx.window.set_focus();
        } else if event.id() == "quit" {
            eprintln!("[exit] uscita dal menu tray: shutdown browser + cancellazione profilo");
            crate::browser::shutdown_and_cleanup_profile();
            std::process::exit(0);
        }
    });

    rsx! {
        head { style { "{TAILWIND_CSS}" } }
        div { class: "h-screen overflow-hidden bg-base-100 p-6",
            div { class: "max-w-2xl mx-auto h-full flex flex-col gap-4",
                header { class: "flex items-center justify-between gap-4",
                    h1 { class: "text-2xl font-bold shrink-0", "pluck v{env!(\"CARGO_PKG_VERSION\")}" }
                    div { class: "flex items-center gap-3",
                        // Spoofing referrer: toggle ON/OFF globale (default ON,
                        // stessi identici modelli di PWDManager — toggle daisyUI)
                        label {
                            class: "flex items-center gap-2.5 rounded-full bg-base-200 px-3.5 py-1.5 shrink-0 cursor-pointer",
                            title: "Spoofing referrer: invia l'header Referer con i download (sostituzione del referrer)",
                            span { class: "text-xs font-semibold uppercase tracking-wide opacity-60", "Spoofing" }
                            input {
                                r#type: "checkbox",
                                class: "toggle spoof-toggle toggle-sm",
                                checked: *spoofing.read(),
                                onchange: move |_| spoofing.toggle(),
                                aria_label: "Spoofing referrer attivo",
                            }
                        }
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| dialog_open.set(true),
                            "＋ Manual"
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
                            "Corrupted referer file ({reason}) — saving disabled. File: "
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
                        on_remove: move |id: DownloadId| {
                            downloads.write().retain(|d| d.id != id);
                            controls.write().remove(&id);
                        },
                        on_clear_all: move |_| {
                            // I download ancora attivi vengono annullati prima di
                            // svuotare la lista: nessuna riga orfana che continua
                            // a scaricare senza controllo. I file su disco non
                            // vengono toccati (il cancel rimuove i parziali).
                            for item in downloads.read().iter() {
                                if !matches!(
                                    item.state,
                                    DownloadState::Completed
                                        | DownloadState::Failed(_)
                                        | DownloadState::Cancelled
                                ) && let Some(ctl) = controls.read().get(&item.id)
                                {
                                    ctl.cancel.cancel();
                                }
                            }
                            downloads.write().clear();
                            controls.write().clear();
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
