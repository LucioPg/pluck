//! Vista "Automatica": browser WebView2 in background (sempre invisibile) + sniffing
//! CDP. L'utente incolla l'URL della pagina; i media rilevati appaiono nella lista.
//!
//! Il browser vive su un thread dedicato e continua a girare anche quando questa
//! tab non è visibile: la raccolta candidati non si ferma mai (scelta utente:
//! background puro, opzione "mostra pagina" scartata).

use crate::browser;
use crate::ui::thumb::Thumb;
use dioxus::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedReceiver;

/// (url, referer, destinazione, thumbnail, is_stream) — richiesta di download
/// dal click su candidato fino a `launch_download`.
pub type DownloadRequest = (String, Option<String>, PathBuf, Option<String>, bool);

#[component]
pub fn AutoPage(
    /// Chiamato al termine della save dialog
    on_download: EventHandler<DownloadRequest>,
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
    // Vero quando l'utente modifica l'URL dopo una navigazione: gli eventi in
    // arrivo dal browser (candidati, titolo, URL) sono stantî e vanno scartati
    // finché il prossimo "Vai" non resetta il flag.
    let url_dirty: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));

    let mut address = use_signal(String::new);
    let candidates = use_signal(Vec::<browser::MediaCandidate>::new);
    let page_url = use_signal(String::new);
    let page_title = use_signal(String::new);
    let load_error = use_signal(|| Option::<String>::None);
    // Vero da "Vai" fino a NavigationCompleted (Title) o errore: feedback processo in corso
    let loading = use_signal(|| false);
    // Pubblicità rilevata dallo scout: finestra estesa, tentativi in corso
    let ad_wait = use_signal(|| false);

    let url_ok = reqwest::Url::parse(address.read().trim()).is_ok();

    let bs_init = browser_state.clone();
    let init_flag = initialized.clone();
    let dirty_init = url_dirty.clone();
    use_effect(move || {
        if !init_flag.get() {
            init_flag.set(true);
            // Clone: la closure è FnMut, non può consumare la cattura
            init_browser(
                bs_init.clone(),
                address,
                candidates,
                page_url,
                page_title,
                load_error,
                loading,
                ad_wait,
                dirty_init.clone(),
                on_error,
            );
        }
    });

    let bs_go = browser_state.clone();
    let dirty_go = url_dirty.clone();
    let init_go = (
        initialized.clone(),
        address,
        candidates,
        page_url,
        page_title,
        load_error,
        loading,
        ad_wait,
        on_error,
    );
    let go = move |_| {
        navigate(&bs_go, &init_go, &dirty_go);
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
                let thumbnail = candidate.thumbnail.clone();
                on_download((
                    candidate.url,
                    if referer.is_empty() { None } else { Some(referer) },
                    PathBuf::from(path),
                    thumbnail,
                    candidate.is_stream,
                ));
            }
            browsing.set(false);
        });
    });

    rsx! {
        div {
            class: "flex flex-col gap-3 mt-3",

            div { class: "flex gap-2",
                button {
                    class: "btn btn-sm",
                    aria_label: "Ricarica pagina",
                    title: "Ricarica: riavvia anche lo sniffing dei media",
                    onclick: {
                        let bs = browser_state.clone();
                        let mut aw = ad_wait;
                        move |_| {
                            // La nuova pagina riparte da zero anche per l'ad
                            aw.set(false);
                            if let Some(handle) = bs.borrow().as_ref() {
                                handle.reload();
                            }
                        }
                    },
                    "⟳"
                }
                input {
                    class: "input input-sm w-full focus:outline-none focus:ring-2 focus:ring-teal-500",
                    r#type: "url",
                    id: "url-input",
                    placeholder: "https://esempio.com/pagina-con-video",
                    value: "{address}",
                    // Riacquisizione del focus su campo pieno: i risultati
                    // vengono svuotati (come il tasto ✕, ma l'URL resta) e il
                    // testo selezionato — il nuovo testo sostituisce il vecchio
                    onfocus: {
                        let url_dirty = url_dirty.clone();
                        move |_| {
                            if !address.read().trim().is_empty() {
                                clear_results(&url_dirty, candidates, page_url, page_title, load_error, loading, ad_wait);
                            }
                            // select() differito: il mouseup che segue il click
                            // cancellerebbe una selezione fatta dentro l'evento focus
                            let _ = dioxus::document::eval(
                                "setTimeout(() => document.getElementById('url-input').select(), 50)",
                            );
                        }
                    },
                    // Qualsiasi modifica/cancellazione dell'URL invalida i
                    // risultati della pagina precedente
                    oninput: {
                        let url_dirty = url_dirty.clone();
                        move |e| {
                            address.set(e.value());
                            clear_results(&url_dirty, candidates, page_url, page_title, load_error, loading, ad_wait);
                        }
                    },
                    onkeydown: {
                        let bs = browser_state.clone();
                        let init = (
                            initialized.clone(),
                            address,
                            candidates,
                            page_url,
                            page_title,
                            load_error,
                            loading,
                            ad_wait,
                            on_error,
                        );
                        let dirty = url_dirty.clone();
                        move |e| {
                            if e.key() == Key::Enter {
                                navigate(&bs, &init, &dirty);
                            }
                        }
                    },
                }
                button {
                    class: "btn btn-sm btn-ghost",
                    aria_label: "Cancella indirizzo",
                    title: "Cancella il campo indirizzo",
                    onclick: {
                        let url_dirty = url_dirty.clone();
                        move |_| {
                            address.set(String::new());
                            clear_results(&url_dirty, candidates, page_url, page_title, load_error, loading, ad_wait);
                        }
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

            // Stato pagina: spinner durante la navigazione, poi titolo + URL
            // correnti (info mentre il browser è nascosto). Ad rilevata → testo
            // dedicato anche a load finito (i tentativi continuano)
            div { class: "text-sm text-base-content/60 truncate flex items-center gap-2",
                if *loading.read() && !*ad_wait.read() {
                    span { class: "loading loading-spinner loading-xs", aria_label: "Caricamento in corso" }
                    span { "Caricamento pagina…" }
                } else if *loading.read() && *ad_wait.read() {
                    span { class: "loading loading-spinner loading-xs", aria_label: "Scansione in corso" }
                    span { "Rilevata pubblicità: attendo il contenuto…" }
                } else if *ad_wait.read() {
                    span { class: "loading loading-spinner loading-xs", aria_label: "Scansione in corso" }
                    span { "Rilevata pubblicità: catturo il contenuto…" }
                } else if page_title.read().trim().is_empty() {
                    "Nessuna pagina caricata"
                } else {
                    "{page_title} — {page_url}"
                }
            }

            div { class: "flex flex-col gap-2 max-h-52 overflow-y-auto pr-1",
                for (i, candidate) in candidates.read().iter().cloned().enumerate() {
                    CandidateRow {
                        key: "{i}-{candidate.url}",
                        index: i,
                        candidate: candidate.clone(),
                        on_download: {
                            let cb = start_download.clone();
                            move |c: browser::MediaCandidate| cb(c)
                        },
                    }
                }
                if candidates.read().is_empty() && load_error.read().is_none() && !*loading.read() {
                    div { class: "text-sm text-base-content/50",
                        "Incolla l'URL di una pagina con un video: i media rilevati appaiono qui. Il browser gira in background: attendi qualche secondo affinché i player carichino i media."
                    }
                }
            }
        }
    }
}

#[component]
fn CandidateRow(
    index: usize,
    candidate: browser::MediaCandidate,
    on_download: EventHandler<browser::MediaCandidate>,
) -> Element {
    let candidate_for_click = candidate.clone();
    let size_label = match candidate.size {
        Some(b) => browser::format_bytes(b),
        None => "—".to_string(),
    };
    rsx! {
        div {
            key: "{index}-{candidate.url}",
            class: "flex items-center gap-2 rounded-lg bg-base-100 px-3 py-1 text-xs shadow-sm",
            // Thumbnail quadrata (poster/og:image) o placeholder
            Thumb { url: candidate.thumbnail.clone() }
            if candidate.is_stream {
                span {
                    class: "badge badge-ghost badge-sm shrink-0",
                    title: "HLS/DASH: convertito in MP4 via ffmpeg",
                    "stream"
                }
            } else {
                span {
                    class: "badge badge-success badge-sm shrink-0",
                    "media"
                }
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
            // Gli stream ora sono scaricabili: ffmpeg remuxa in MP4
            button {
                class: "btn btn-xs btn-primary shrink-0",
                onclick: move |_| on_download.call(candidate_for_click.clone()),
                "Scarica"
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
    Signal<bool>,
    Signal<bool>,
    EventHandler<String>,
);

/// Svuota i risultati della pagina corrente (candidati, titolo/URL, errori) e
/// marca gli eventi del browser come stantî: mentre l'utente modifica l'URL,
/// scout e CDP continuerebbero a produrre dati della pagina vecchia.
/// Ferma anche lo spinner: il Title della pagina vecchia verrà scartato.
fn clear_results(
    url_dirty: &Rc<Cell<bool>>,
    mut candidates: Signal<Vec<browser::MediaCandidate>>,
    mut page_url: Signal<String>,
    mut page_title: Signal<String>,
    mut load_error: Signal<Option<String>>,
    mut loading: Signal<bool>,
    mut ad_wait: Signal<bool>,
) {
    url_dirty.set(true);
    candidates.write().clear();
    page_url.set(String::new());
    page_title.set(String::new());
    load_error.set(None);
    loading.set(false);
    ad_wait.set(false);
}

/// Naviga: init lazy se necessario, reset candidati/stato, comando Navigate.
/// L'init qui è la cintura di sicurezza: se l'use_effect non si ri-attiva al
/// cambio tab (tracking), il primo "Vai" inita comunque il browser.
fn navigate(
    browser_state: &Rc<RefCell<Option<browser::BrowserHandle>>>,
    init: &InitState,
    url_dirty: &Rc<Cell<bool>>,
) {
    let (initialized, address, candidates, page_url, page_title, load_error, loading, ad_wait, on_error) = init;
    // Copie dei Signal: set/write vogliono binding mut (KB dioxus)
    let address = *address;
    let mut candidates = *candidates;
    let mut page_title = *page_title;
    let mut load_error = *load_error;
    let mut loading = *loading;
    let mut ad_wait = *ad_wait;
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
            loading,
            ad_wait,
            url_dirty.clone(),
            *on_error,
        );
    }
    let url = address.read().trim().to_string();
    if reqwest::Url::parse(&url).is_err() {
        return;
    }
    load_error.set(None);
    candidates.write().clear();
    page_title.set(String::new());
    // Riaccetta gli eventi del browser: da qui in poi sono della nuova pagina
    // (subito, prima del comando Navigate, per non perdere i primi eventi)
    url_dirty.set(false);
    loading.set(true);
    ad_wait.set(false);
    match browser_state.borrow().as_ref() {
        Some(handle) => {
            eprintln!("[auto] navigate: {url}");
            handle.navigate(&url);
        }
        None => eprintln!("[auto] ATTENZIONE: handle browser assente, navigate ignorata"),
    }
}

/// Init lazy: thread WebView2 + task consumatore eventi (candidati, URL, titolo,
/// errori → toast). Con `url_dirty` attivo gli eventi sono stantî (l'utente ha
/// modificato l'URL dopo l'ultima navigazione) e vengono scartati.
#[allow(clippy::too_many_arguments)]
fn init_browser(
    browser_state: Rc<RefCell<Option<browser::BrowserHandle>>>,
    mut address: Signal<String>,
    mut candidates: Signal<Vec<browser::MediaCandidate>>,
    mut page_url: Signal<String>,
    mut page_title: Signal<String>,
    mut load_error: Signal<Option<String>>,
    mut loading: Signal<bool>,
    mut ad_wait: Signal<bool>,
    url_dirty: Rc<Cell<bool>>,
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
    // Handle condiviso per lo shutdown all'uscita (cancellazione profilo)
    browser::register_shared_browser(&handle);
    *browser_state.borrow_mut() = Some(handle);

    spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            // URL modificato dall'utente dopo l'ultima navigazione: tutto ciò
            // che il browser produce riguarda la pagina vecchia → scartato
            if url_dirty.get() {
                continue;
            }
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
                    page_title.set(title);
                    // NavigationCompleted: la pagina è caricata, lo spinner può
                    // sparire anche se il titolo è vuoto
                    loading.set(false);
                    ad_wait.set(false);
                }
                browser::BrowserEvent::AdWait(active) => {
                    eprintln!("[auto] ad scout esteso: attivo={active}");
                    ad_wait.set(active);
                }
                browser::BrowserEvent::PageThumb(thumb) => {
                    // Backfill: candidati CDP arrivati prima dell'estrazione
                    // (e senza poster proprio) prendono la thumb di pagina
                    eprintln!("[auto] thumbnail di pagina ricevuta: backfill candidati");
                    let mut list = candidates.write();
                    for c in list.iter_mut() {
                        if c.thumbnail.is_none() {
                            c.thumbnail = Some(thumb.clone());
                        }
                    }
                }
                browser::BrowserEvent::Error(msg) => {
                    eprintln!("[auto] ERRORE browser: {msg}");
                    load_error.set(Some(msg.clone()));
                    loading.set(false);
                    ad_wait.set(false);
                    on_error.call(msg);
                }
            }
        }
        eprintln!("[auto] canale eventi chiuso (thread browser terminato?)");
    });
}
