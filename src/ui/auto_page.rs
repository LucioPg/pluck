//! Vista "Automatica": browser WebView2 in background (sempre invisibile) + sniffing
//! CDP. L'utente incolla l'URL della pagina; i media rilevati appaiono nella lista.
//!
//! Il browser vive su un thread dedicato e continua a girare anche quando questa
//! tab non è visibile: la raccolta candidati non si ferma mai (scelta utente:
//! background puro, opzione "mostra pagina" scartata).

use crate::browser;
use dioxus::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedReceiver;

#[component]
pub fn AutoPage(
    /// (url, referer, destinazione) — chiamato al termine della save dialog
    on_download: EventHandler<(String, Option<String>, PathBuf)>,
    /// Toast errori (init WebView2, save dialog)
    on_error: EventHandler<String>,
) -> Element {
    // Browser avviato al mount: la vista è sempre visibile (pagina unica).
    let browser_state: Rc<RefCell<Option<browser::BrowserHandle>>> =
        use_hook(|| Rc::new(RefCell::new(None)));
    let initialized = use_hook(|| Rc::new(Cell::new(false)));
    // Guard anti doppio-click sulla save dialog (Cell: basta, il dialog modale blocca
    // comunque le interazioni; nessuna reattività necessaria)
    let browsing: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));

    let mut address = use_signal(String::new);
    let candidates = use_signal(Vec::<browser::MediaCandidate>::new);
    let page_url = use_signal(String::new);
    let page_title = use_signal(String::new);
    let load_error = use_signal(|| Option::<String>::None);

    let url_ok = reqwest::Url::parse(address.read().trim()).is_ok();

    let bs_init = browser_state.clone();
    let init_flag = initialized.clone();
    use_effect(move || {
        if !init_flag.get() {
            init_flag.set(true);
            init_browser(
                bs_init.clone(),
                address,
                candidates,
                page_url,
                page_title,
                load_error,
                on_error,
            );
        }
    });

    let bs_go = browser_state.clone();
    let init_go = (
        initialized.clone(),
        address,
        candidates,
        page_url,
        page_title,
        load_error,
        on_error,
    );
    let go = move |_| {
        navigate(&bs_go, &init_go);
    };

    // Click su candidato: save dialog nativa precompilata → download.
    // Rc<dyn Fn>:Fn (solo letture + Cell guard) clonabile per ogni riga.
    let start_download: Rc<dyn Fn(browser::MediaCandidate)> = Rc::new(move |candidate| {
        if browsing.get() {
            return;
        }
        browsing.set(true);
        let suggested = {
            let raw = if page_title.read().trim().is_empty() {
                candidate.url.rsplit('/').next().unwrap_or("video").to_string()
            } else {
                page_title.read().trim().to_string()
            };
            format!(
                "{}{}",
                browser::sanitize_filename(&raw),
                browser::extension_from_url(&candidate.url)
            )
        };
        let referer = page_url.read().trim().to_string();
        let browsing = browsing.clone();
        spawn(async move {
            let dialog = rfd::AsyncFileDialog::new()
                .add_filter("MP4", &["mp4"])
                .add_filter("Tutti i file", &["*"])
                .set_file_name(&suggested);
            if let Some(handle) = dialog.save_file().await
                && let Some(path) = handle.path().to_str()
            {
                on_download((
                    candidate.url,
                    if referer.is_empty() { None } else { Some(referer) },
                    PathBuf::from(path),
                ));
            }
            browsing.set(false);
        });
    });

    let only_streams =
        !candidates.read().is_empty() && candidates.read().iter().all(|c| c.is_stream);

    rsx! {
        div {
            class: "flex flex-col gap-3",

            div { class: "flex gap-2",
                button {
                    class: "btn btn-sm",
                    aria_label: "Ricarica pagina",
                    title: "Ricarica: riavvia anche lo sniffing dei media",
                    onclick: {
                        let bs = browser_state.clone();
                        move |_| {
                            if let Some(handle) = bs.borrow().as_ref() {
                                handle.reload();
                            }
                        }
                    },
                    "⟳"
                }
                input {
                    class: "input input-sm w-full",
                    r#type: "url",
                    placeholder: "https://esempio.com/pagina-con-video",
                    value: "{address}",
                    oninput: move |e| address.set(e.value()),
                    onkeydown: {
                        let bs = browser_state.clone();
                        let init = (
                            initialized.clone(),
                            address,
                            candidates,
                            page_url,
                            page_title,
                            load_error,
                            on_error,
                        );
                        move |e| {
                            if e.key() == Key::Enter {
                                navigate(&bs, &init);
                            }
                        }
                    },
                }
                button {
                    class: "btn btn-sm btn-ghost",
                    aria_label: "Cancella indirizzo",
                    title: "Cancella il campo indirizzo",
                    onclick: move |_| {
                        address.set(String::new());
                    },
                    "✕"
                }
                button {
                    class: "btn btn-sm btn-primary",
                    disabled: address.read().trim().is_empty() || !url_ok,
                    onclick: go,
                    "Vai"
                }
            }

            if let Some(err) = load_error.read().as_ref() {
                div { class: "alert alert-error text-sm", span { "{err}" } }
            }

            // Stato pagina: titolo + URL correnti (info mentre il browser è nascosto)
            div { class: "text-sm text-base-content/60 truncate",
                if page_title.read().trim().is_empty() {
                    "Nessuna pagina caricata"
                } else {
                    "{page_title} — {page_url}"
                }
            }

            if only_streams {
                div { class: "alert alert-warning text-sm",
                    span { "Solo video in streaming (m3u8/mpd) rilevati: download non supportato — aggiornamento futuro" }
                }
            }

            div { class: "flex flex-col gap-2 max-h-52 overflow-y-auto pr-1",
                for (i, candidate) in candidates.read().iter().cloned().enumerate() {
                    { candidate_row(i, candidate, start_download.clone()) }
                }
                if candidates.read().is_empty() && load_error.read().is_none() {
                    div { class: "text-sm text-base-content/50",
                        "Incolla l'URL di una pagina con un video: i media rilevati appaiono qui. Il browser gira in background: attendi qualche secondo affinché i player carichino i media."
                    }
                }
            }
        }
    }
}

fn candidate_row(
    index: usize,
    candidate: browser::MediaCandidate,
    start_download: Rc<dyn Fn(browser::MediaCandidate)>,
) -> Element {
    // Due copie: una per il display (prendibile a prestito dal RSX), una per la
    // closure del click (consumata)
    let candidate_for_click = candidate.clone();
    let size_label = match candidate.size {
        Some(b) => browser::format_bytes(b),
        None => "—".to_string(),
    };
    rsx! {
        div {
            key: "{index}-{candidate.url}",
            class: "flex items-center gap-2 rounded-lg bg-base-100 px-3 py-1 text-xs shadow-sm",
            span {
                class: if candidate.is_stream {
                    "badge badge-warning badge-sm shrink-0"
                } else {
                    "badge badge-success badge-sm shrink-0"
                },
                if candidate.is_stream { "m3u8/mpd" } else { "media" }
            }
            // Nome leggibile (tooltip = URL completo)
            span {
                class: "truncate min-w-0 flex-1 font-medium",
                title: "{candidate.url}",
                "{candidate.name}"
            }
            // Dimensione se nota (probe HEAD/CDP), altrimenti trattino
            span { class: "tabular-nums opacity-70 shrink-0 w-20 text-right",
                title: "{candidate.url}",
                "{size_label}"
            }
            if candidate.is_stream {
                span {
                    class: "badge badge-ghost badge-sm shrink-0",
                    title: "Non scaricabile: aggiornamento futuro",
                    "non supp."
                }
            } else {
                button {
                    class: "btn btn-xs btn-primary shrink-0",
                    onclick: move |_| start_download(candidate_for_click.clone()),
                    "Scarica"
                }
            }
        }
    }
}

/// Naviga: reset candidati/stato e comando Navigate al thread browser.
/// Stato necessario all'init lazy, condiviso tra effect e navigate.
type InitState = (
    Rc<Cell<bool>>,
    Signal<String>,
    Signal<Vec<browser::MediaCandidate>>,
    Signal<String>,
    Signal<String>,
    Signal<Option<String>>,
    EventHandler<String>,
);

/// Naviga: init lazy se necessario, reset candidati/stato, comando Navigate.
/// L'init qui è la cintura di sicurezza: se l'use_effect non si ri-attiva al
/// cambio tab (tracking), il primo "Vai" inita comunque il browser.
fn navigate(
    browser_state: &Rc<RefCell<Option<browser::BrowserHandle>>>,
    init: &InitState,
) {
    let (initialized, address, candidates, page_url, page_title, load_error, on_error) = init;
    // Copie dei Signal: set/write vogliono binding mut (KB dioxus)
    let address = *address;
    let mut candidates = *candidates;
    let mut page_title = *page_title;
    let mut load_error = *load_error;
    if browser_state.borrow().is_none() {
        if initialized.get() {
            eprintln!("[auto] ATTENZIONE: init segnato done ma handle assente — reinizo");
        }
        initialized.set(true);
        eprintln!("[auto] init browser (lazy da navigate)");
        init_browser(
            browser_state.clone(),
            address,
            candidates,
            *page_url,
            page_title,
            load_error,
            *on_error,
        );
    }
    let url = address.read().trim().to_string();
    if reqwest::Url::parse(&url).is_err() {
        return;
    }
    load_error.set(None);
    candidates.write().clear();
    page_title.set(String::new());    match browser_state.borrow().as_ref() {
        Some(handle) => {
            eprintln!("[auto] navigate: {url}");
            handle.navigate(&url);
        }
        None => eprintln!("[auto] ATTENZIONE: handle browser assente, navigate ignorata"),
    }
}

/// Init lazy: thread WebView2 + task consumatore eventi (candidati, URL, titolo,
/// errori → toast).
#[allow(clippy::too_many_arguments)]
fn init_browser(
    browser_state: Rc<RefCell<Option<browser::BrowserHandle>>>,
    mut address: Signal<String>,
    mut candidates: Signal<Vec<browser::MediaCandidate>>,
    mut page_url: Signal<String>,
    mut page_title: Signal<String>,
    mut load_error: Signal<Option<String>>,
    on_error: EventHandler<String>,
) {
    let dx = dioxus::desktop::window();
    // tao Window → hwnd (isize, alias di tao su Windows); tao re-esportato da dioxus-desktop
    use dioxus::desktop::tao::platform::windows::WindowExtWindows;
    let hwnd = dx.window.hwnd();
    eprintln!("[auto] init_browser: hwnd={hwnd}");

    let (event_tx, mut event_rx): (
        tokio::sync::mpsc::UnboundedSender<browser::BrowserEvent>,
        UnboundedReceiver<browser::BrowserEvent>,
    ) = tokio::sync::mpsc::unbounded_channel();

    let handle = browser::controller::spawn_browser(hwnd, browser::profile_dir(), event_tx);
    *browser_state.borrow_mut() = Some(handle);

    spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            match ev {
                browser::BrowserEvent::Candidate(c) => {
                    eprintln!("[auto] candidato ricevuto: {} (stream={})", c.url, c.is_stream);
                    let mut list = candidates.write();
                    list.insert(0, c.clone());
                    list.truncate(200);
                    drop(list);
                    // Dimensione mancante (candidato DOM) → probe HEAD async
                    // con referer = pagina corrente; aggiorna la riga al termine
                    if c.size.is_none() && !c.is_stream {
                        let referer = page_url.read().trim().to_string();
                        let mut candidates_updater = candidates;
                        spawn(async move {
                            if let Some(size) =
                                browser::probe_size(&c.url, Some(referer.as_str()).filter(|r| !r.is_empty())).await
                            {
                                let mut list = candidates_updater.write();
                                if let Some(item) =
                                    list.iter_mut().find(|i| i.url == c.url)
                                {
                                    item.size = Some(size);
                                }
                            }
                        });
                    }
                }
                browser::BrowserEvent::PageUrl(url) => {
                    eprintln!("[auto] pagina corrente: {url}");
                    page_url.set(url.clone());
                    // La address bar segue la navigazione del browser
                    address.set(url);
                }
                browser::BrowserEvent::Title(title) => {
                    eprintln!("[auto] titolo ricevuto: {title}");
                    page_title.set(title)
                }
                browser::BrowserEvent::Error(msg) => {
                    eprintln!("[auto] ERRORE browser: {msg}");
                    load_error.set(Some(msg.clone()));
                    on_error.call(msg);
                }
            }
        }
        eprintln!("[auto] canale eventi chiuso (thread browser terminato?)");
    });
}
