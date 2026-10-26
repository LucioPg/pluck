//! Thread browser: crea WebView2 su un thread dedicato (pump di messaggi proprio)
//! e inoltra gli eventi all'UI via canale tokio.
//!
//! Regole architetturali (spec §4):
//! - TUTTE le chiamate COM avvengono su questo thread — l'UI non tocca mai COM
//! - L'init usa `wait_for_async_operation` (nested pump): sicuro perché qui non
//!   c'è nessun loop UI in esecuzione (il reentrancy problem è solo sul main)
//! - I comandi arrivano da un canale std mpsc, consumato a ogni giro di pump

use super::sniff::{CandidateSet, candidate_from_response_received, classify_dom};
use super::{BrowserCommand, BrowserEvent, BrowserHandle};
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, CoreWebView2EnvironmentOptions,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    DevToolsProtocolEventReceivedEventHandler, DownloadStartingEventHandler,
    ExecuteScriptCompletedHandler, Microsoft::Web::WebView2::Win32::*,
    NavigationCompletedEventHandler, NewWindowRequestedEventHandler,
    PermissionRequestedEventHandler, SourceChangedEventHandler, take_pwstr,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, RECT},
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
        System::Threading::{GetCurrentThreadId, OpenProcess, PROCESS_TERMINATE, TerminateProcess},
        UI::WindowsAndMessaging::{
            DispatchMessageW, EnumChildWindows, GetClientRect, GetWindowThreadProcessId,
            HWND_BOTTOM, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
            PeekMessageW, QS_ALLINPUT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
            TranslateMessage,
        },
    },
    core::{HSTRING, Interface, PCWSTR, PWSTR},
};

/// Log diagnostico esteso (raw CDP, tick scout vuoti): rumore per l'uso normale.
/// Attivo con variabile d'ambiente LDW_VERBOSE=1 (debug sniffing).
fn verbose_logs() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("LDW_VERBOSE")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// Estrazione titolo + thumbnail di pagina (ritorna JSON `{t, th}`). Priorita'
/// titolo all'elemento vicino al player. Chain completa:
/// 1. attributi title/data-title del <video>
/// 2. heading/p precedente al <video> (documento del player, iframe ricorsivi)
/// 3. heading [id/class*="title"] risalendo i padri del player
/// 4. og:title -> twitter:title -> document.title pulito
/// 5. h1/h2 FILTRATI anti-disclaimer (per ultimo: il primo h1 e' spesso il
///    popup/descrizione-eta', caso tranny.one #lawH1 prima di #mTitle)
const TITLE_JS: &str = r#"
(function() {
    const __title = (function() {
    const BAD_TEXT = /(age[- ]restrict|this website|warning|disclaimer|pornograph|materials may|consent|18\+|entering|by entering)/i;
    const BAD_SIG = /(law|modal|popup|overlay|consent|agegate|age-gate|disclaimer)/i;
    function okEl(el) {
        if (!el) return false;
        const t = el.textContent.trim();
        if (!t || t.length > 150) return false;
        if (BAD_TEXT.test(t)) return false;
        const sig = (el.id || '') + ' ' + (el.className || '');
        return !BAD_SIG.test(sig);
    }
    function fromVideo(doc) {
        if (!doc) return null;
        for (const v of doc.querySelectorAll('video')) {
            if (v.title && v.title.trim()) return v.title.trim();
            const dt = v.getAttribute('data-title') || v.getAttribute('aria-label');
            if (dt && dt.trim()) return dt.trim();
            let el = v;
            for (let i = 0; i < 5; i++) {
                el = el.previousElementSibling;
                if (!el) break;
                if (/^(P|H[1-6])$/.test(el.tagName) && el.textContent.trim() && okEl(el)) {
                    return el.textContent.trim();
                }
            }
            let p = v.parentElement;
            for (let d = 0; p && d < 5; d++, p = p.parentElement) {
                const t = p.querySelector(
                    'h1[id*="title" i], h2[id*="title" i], h3[id*="title" i], ' +
                    '[id*="mtitle" i], [class*="movie-title" i], [class*="video-title" i], ' +
                    'h1[class*="title" i]'
                );
                if (t && okEl(t)) return t.textContent.trim();
            }
        }
        for (const f of doc.querySelectorAll('iframe')) {
            try {
                if (f.contentDocument) {
                    const t = fromVideo(f.contentDocument);
                    if (t) return t;
                }
            } catch (e) { /* cross-origin */ }
        }
        return null;
    }
    const near = fromVideo(document);
    if (near) return near;
    function clean(t) {
        t = (t || '').trim();
        let i = t.lastIndexOf('|');
        if (i > 0) t = t.slice(0, i).trim();
        i = t.lastIndexOf(' - ');
        if (i > 0) t = t.slice(0, i).trim();
        i = t.lastIndexOf('–');
        if (i > 0) t = t.slice(0, i).trim();
        return t;
    }
    const og = document.querySelector('meta[property="og:title"]');
    if (og && og.content && og.content.trim()) return og.content.trim();
    const tw = document.querySelector('meta[name="twitter:title"]');
    if (tw && tw.content && tw.content.trim()) return tw.content.trim();
    const dt = clean(document.title);
    if (dt) return dt;
    for (const h of document.querySelectorAll('h1, h2')) {
        if (okEl(h)) return h.textContent.trim();
    }
    const any = document.querySelector('h1');
    if (any) return any.textContent.trim();
    return '';
    })();
    // Thumbnail di pagina: og:image → twitter:image → poster del primo <video>
    let __h = '';
    const ogi = document.querySelector('meta[property="og:image"]');
    if (ogi && ogi.content && ogi.content.trim()) __h = ogi.content.trim();
    if (!__h) {
        const twi = document.querySelector('meta[name="twitter:image"]');
        if (twi && twi.content && twi.content.trim()) __h = twi.content.trim();
    }
    if (!__h) {
        const v = document.querySelector('video');
        if (v && v.poster) __h = v.poster;
    }
    return JSON.stringify({ t: __title, th: __h });
})()
"#;

/// Scout DOM — TICK istantaneo guidato da Rust: il pump loop lo lancia ogni 1s
/// dopo ogni navigazione (per 20s, estesi finché è attiva una pubblicità).
/// Stato in window.__lwSeen (persiste nella pagina): ogni tick ritorna SOLO i
/// nuovi URL trovati + flag ad_active. RICORSIVO negli iframe same-origin (il
/// player è spesso una pagina embeddata — es. player-x.php); cross-origin →
/// contentDocument null → skip (quelli li vede il CDP). Il play muted avviene
/// una volta per video; se è stata vista una pubblicità, i video pausi vengono
/// riavviati a ogni tick (pre-roll finito → riparte il contenuto). Niente
/// Promise né postMessage.
const SCOUT_TICK_JS: &str = r#"
(function() {
    if (!window.__lwSeen) { window.__lwSeen = {}; window.__lwPlayed = {}; window.__lwHadAd = false; }
    const out = [];
    if (!window.__lwDiag) {
        window.__lwDiag = true;
        try {
            const vids = [...document.querySelectorAll('video')].map(v => ({
                src: (v.currentSrc || v.src || '').slice(0, 70),
                paused: v.paused, rs: v.readyState, ns: v.networkState,
                err: v.error ? (v.error.code + ' ' + v.error.message).slice(0, 90) : null,
                buf: v.buffered.length ? Math.round(v.buffered.end(v.buffered.length - 1)) : 0
            }));
            const res = performance.getEntriesByType('resource')
                .map(r => r.name)
                .filter(n => /vcdn|ahcdn|m3u8|\.ts(\?|$)|segment/i.test(n))
                .map(n => n.slice(0, 120)).slice(0, 12);
            out.push({ url: '__diag__' + JSON.stringify({ vids: vids, res: res, hls: typeof window.Hls }), from_video: false, thumbnail: '' });
        } catch (e) { out.push({ url: '__diag__err:' + e, from_video: false, thumbnail: '' }); }
    }
    const AD_URL = /(doubleclick|googlesyndication|adservice|adsystem|adcreative|adserver|\/ads?\/)/i;
    const AD_DOM = /(ad[-_]?showing|ytp-ad|jw-ad|ad-overlay|adsbox|advertisement|ad-container|ad-banner)/i;
    let adActive = false;
    function adLike(v) {
        if (AD_URL.test(v.currentSrc || v.src || '')) return true;
        // Durata nota e corta = quasi sicuramente pubblicità (i contenuti sono
        // più lunghi; NaN/Infinity = live o metadata assenti → non conta)
        const d = v.duration;
        return !isNaN(d) && d > 0 && d < 60 && !v.ended;
    }
    function collect(doc) {
        if (!doc) return;
        doc.querySelectorAll('video, video source').forEach(el => {
            const u = el.currentSrc || el.src || el.getAttribute('src') || '';
            if (u && !window.__lwSeen[u]) {
                window.__lwSeen[u] = 1;
                const v = el.tagName === 'VIDEO' ? el : el.closest('video');
                out.push({ url: u, from_video: true, thumbnail: (v && v.poster) || '' });
            }
        });
        doc.querySelectorAll('a[href]').forEach(a => {
            const h = a.getAttribute('href') || '';
            if (/\.(mp4|webm|mkv|mov|avi|m4v|mpg|mpeg|ts)(\?|#|$)/i.test(h) && !window.__lwSeen[h]) {
                window.__lwSeen[h] = 1;
                const im = a.querySelector('img');
                out.push({ url: h, from_video: false, thumbnail: (im && im.src) || '' });
            }
        });
        doc.querySelectorAll('video').forEach(v => {
            const src = v.currentSrc || v.src || '';
            const key = (doc === document ? '' : 'i:') + src;
            if (adLike(v)) { adActive = true; window.__lwHadAd = true; }
            if (!adActive) {
                let el = v;
                for (let d = 0; el && d < 3; d++, el = el.parentElement) {
                    const sig = (el.id || '') + ' ' + (el.className || '');
                    if (AD_DOM.test(sig)) { adActive = true; window.__lwHadAd = true; break; }
                }
            }
            // Play muted: una volta per src; se è passata una pubblicità,
            // riavvia anche i video già giocati (contenuto dopo il pre-roll)
            if (v.paused && src && (!window.__lwPlayed[key] || window.__lwHadAd)) {
                window.__lwPlayed[key] = 1;
                v.muted = true;
                const p = v.play();
                if (p && p.catch) p.catch(e => out.push({ url: '__play_error__:' + (e && e.name || 'unknown'), from_video: false, thumbnail: '' }));
            }
        });
        doc.querySelectorAll('iframe').forEach(f => {
            try { collect(f.contentDocument); } catch (e) { /* cross-origin */ }
        });
    }
    collect(document);
    return JSON.stringify({ items: out, ad_active: adActive });
})()
"#;

/// Avvia il thread browser. `events` riceve candidati/URL/titolo/errori; il caller
/// deve consumarlo (tipicamente un task dentro il componente Dioxus).
pub fn spawn_browser(
    parent_hwnd: isize,
    user_data_dir: PathBuf,
    events: UnboundedSender<BrowserEvent>,
) -> BrowserHandle {
    let (tx, rx) = std::sync::mpsc::channel::<BrowserCommand>();
    std::thread::Builder::new()
        .name("webview2-browser".into())
        .spawn(move || {
            browser_thread(parent_hwnd, user_data_dir, events, rx);
        })
        .expect("spawn browser thread");
    BrowserHandle { tx }
}

fn browser_thread(
    parent_hwnd: isize,
    user_data_dir: PathBuf,
    events: UnboundedSender<BrowserEvent>,
    rx: std::sync::mpsc::Receiver<BrowserCommand>,
) {
    // Tutto il corpo è COM: un solo blocco unsafe (edition 2024: le unsafe fn
    // non permettono più op unsafe implicite nel body)
    unsafe {
        eprintln!(
            "[browser] thread avviato: hwnd={parent_hwnd}, profilo={}, verbose={}",
            user_data_dir.display(),
            verbose_logs()
        );
        // S_FALSE (già inizializzata) arriva come Ok: solo gli errori veri bloccano
        if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
            eprintln!("[browser] ERRORE: CoInitializeEx fallita");
            let _ = events.send(BrowserEvent::Error(
                "CoInitializeEx: COM non disponibile".into(),
            ));
            return;
        }

        let options = CoreWebView2EnvironmentOptions::default();
        // Lingua fissa en-US: i siti con subdomain localizzato (it.X.com) devono
        // restare sul default (scelta utente), non seguire la lingua di Windows
        options.set_language("en-US".into());
        // Autoplay senza user gesture: i player HLS (hls.js) avviano il load dei
        // segmenti solo dopo un play() che nell'WebView2 invisibile viene negato
        // (preload assente → zero fetch → zero eventi CDP, caso siti tube con player HLS)
        // + niente occlusion tracking: il browser sta dietro l'UI dell'app e
        // Chromium altrimenti lo tratterebbe come occluso → pagina "hidden" →
        // i player con visibility-gating non caricano mai (stesso sintomo)
        options.set_additional_browser_arguments(
            "--autoplay-policy=no-user-gesture-required \
         --disable-features=CalculateNativeWinOcclusion"
                .into(),
        );

        let environment = {
            let (tx, rx) = std::sync::mpsc::channel();
            eprintln!("[browser] creazione environment (profilo dedicato)…");
            if let Err(e) = CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| {
                    CreateCoreWebView2EnvironmentWithOptions(
                        PCWSTR::null(),
                        &HSTRING::from(user_data_dir.as_os_str()),
                        &ICoreWebView2EnvironmentOptions::from(options),
                        &handler,
                    )
                    .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |error_code, environment| {
                    error_code?;
                    tx.send(environment)
                        .expect("send environment over mpsc channel");
                    Ok(())
                }),
            ) {
                let _ = events.send(BrowserEvent::Error(format!("Runtime WebView2: {e}")));
                return;
            }
            match rx.recv() {
                Ok(Some(env)) => {
                    eprintln!("[browser] environment creato");
                    env
                }
                _ => {
                    eprintln!("[browser] ERRORE: environment nullo");
                    let _ = events.send(BrowserEvent::Error("WebView2: null environment".into()));
                    return;
                }
            }
        };

        let controller = {
            let (tx, rx) = std::sync::mpsc::channel();
            eprintln!("[browser] creazione controller su HWND padre…");
            if let Err(e) = CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| {
                    environment
                        .CreateCoreWebView2Controller(HWND(parent_hwnd as _), &handler)
                        .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |error_code, controller| {
                    error_code?;
                    tx.send(controller)
                        .expect("send controller over mpsc channel");
                    Ok(())
                }),
            ) {
                let _ = events.send(BrowserEvent::Error(format!("Controller WebView2: {e}")));
                return;
            }
            match rx.recv() {
                Ok(Some(ctl)) => {
                    eprintln!("[browser] controller creato");
                    ctl
                }
                _ => {
                    eprintln!("[browser] ERRORE: controller nullo");
                    let _ = events.send(BrowserEvent::Error("WebView2: null controller".into()));
                    return;
                }
            }
        };

        // Invisibile = document.hidden=true → i player tube-site
        // gate-ano l'avvio HLS sulla visibilità e non caricano MAI il manifest.
        // Soluzione stealth: controller VISIBILE (pagina "visible", player parte)
        // ma gli HWND figli del browser finiscono in fondo allo Z-order, coperti
        // dalla webview dell'app → l'utente non vede mai il browser.
        let _ = controller.SetIsVisible(true);
        let mut client = RECT::default();
        let _ = GetClientRect(HWND(parent_hwnd as _), &mut client);
        let _ = controller.SetBounds(client);
        push_browser_windows_below(parent_hwnd);

        let Ok(webview) = controller.CoreWebView2() else {
            eprintln!("[browser] ERRORE: CoreWebView2 assente");
            let _ = events.send(BrowserEvent::Error("WebView2: CoreWebView2 missing".into()));
            return;
        };
        eprintln!("[browser] CoreWebView2 pronto");

        // Mute a livello browser: il player NON può smutarsi da JS (vale anche per
        // le pubblicità). Scout ri-forza video.muted a ogni tick come cintura.
        if let Ok(wv15) = webview.cast::<ICoreWebView2_15>() {
            let _ = wv15.SetIsMuted(true);
            eprintln!("[browser] webview muta (ICoreWebView2_15)");
        } else {
            eprintln!("[browser] ATTENZIONE: ICoreWebView2_15 assente, niente mute globale");
        }

        // Pid del processo browser: serve allo shutdown (TerminateProcess veloce —
        // il Close() graceful richiede secondi di teardown e tiene il lock sul
        // profilo, bloccando la cancellazione privacy all'uscita)
        let mut browser_pid: u32 = 0;
        let _ = webview.BrowserProcessId(&mut browser_pid);
        eprintln!("[browser] processo browser pid={browser_pid}");

        // Politiche di sicurezza (spec §2): niente popup, permessi negati, download
        // nativi annullati (il download passa da noi), NESSUN host object esposto.
        let _ = webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_webview, args| {
                if let Some(args) = args {
                    let _ = args.SetHandled(true);
                }
                Ok(())
            })),
            &mut 0,
        );
        let _ = webview.add_PermissionRequested(
            &PermissionRequestedEventHandler::create(Box::new(|_webview, args| {
                if let Some(args) = args {
                    let _ = args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY);
                }
                Ok(())
            })),
            &mut 0,
        );
        // DownloadStarting è su ICoreWebView2_4: cast necessario
        if let Ok(webview4) = webview.cast::<ICoreWebView2_4>() {
            let _ = webview4.add_DownloadStarting(
                &DownloadStartingEventHandler::create(Box::new(|_webview, args| {
                    if let Some(args) = args {
                        let _ = args.SetHandled(true);
                    }
                    Ok(())
                })),
                &mut 0,
            );
        }

        // URL pagina corrente (referer per i download)
        let url_events = events.clone();
        // Dedup CONDIVISO tra scout DOM e CDP (possono vedere lo stesso URL):
        // creato PRIMA delle registrazioni che lo catturano
        let candidates = std::rc::Rc::new(std::cell::RefCell::new(CandidateSet::new(500)));
        // Thumbnail di pagina corrente (og:image/twitter:image/poster): estratta a
        // ogni NavigationCompleted, attaccata come fallback ai candidati CDP (che
        // non hanno contesto DOM). Reset a ogni navigazione (exec).
        let page_thumb: std::rc::Rc<std::cell::RefCell<Option<String>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let _ = webview.add_SourceChanged(
            &SourceChangedEventHandler::create(Box::new(move |webview, _args| {
                if let Some(webview) = webview {
                    let mut uri = PWSTR::null();
                    if webview.Source(&mut uri).is_ok() {
                        let url = take_pwstr(uri);
                        eprintln!("[browser] SourceChanged: {url}");
                        let _ = url_events.send(BrowserEvent::PageUrl(url));
                    }
                }
                Ok(())
            })),
            &mut 0,
        );

        // Titolo dopo ogni navigazione completata + attivazione scout (§5)
        let title_events = events.clone();
        let title_page_thumb = page_thumb.clone();
        let scout_webview = webview.clone();
        let scout_deadline =
            std::rc::Rc::new(std::cell::Cell::new(Option::<std::time::Instant>::None));
        let scout_last = std::rc::Rc::new(std::cell::Cell::new(Option::<std::time::Instant>::None));
        // Ancoraggio della navigazione corrente: cap totale di estensioni scout
        // per la pubblicità (90s dalla navigazione, poi si rinuncia)
        let scout_since =
            std::rc::Rc::new(std::cell::Cell::new(Option::<std::time::Instant>::None));
        // AdWait(true) già inviato all'UI per questa finestra: evita un evento a
        // ogni tick mentre la pubblicità gira
        let ad_notified = std::rc::Rc::new(std::cell::Cell::new(false));
        // Stato ad corrente lato Rust (ultimo tick scout): i candidati CDP che
        // arrivano fuori dai tick lo leggono per decidere se armare lo shutdown
        let ad_active_now = std::rc::Rc::new(std::cell::Cell::new(false));
        // Chiusura post-cattura: primo candidato (fuori finestra ad) → 3s di grazia
        // per raccogliere le varianti di qualità, poi TerminateProcess del browser.
        // Riazzerato a ogni nuovo candidato; disarmato mentre la pubblicità gira.
        // ARM SOLO DOPO NavigationCompleted: i candidati preload di hls.js
        // arrivano spesso prima del completamento — armare lì farebbe scadere il
        // timer prima del Title (titolo/thumbnail persi).
        const CAPTURE_GRACE: std::time::Duration = std::time::Duration::from_secs(3);
        let capture_deadline =
            std::rc::Rc::new(std::cell::Cell::new(Option::<std::time::Instant>::None));
        let nav_completed = std::rc::Rc::new(std::cell::Cell::new(false));
        // Titolo non ancora trovato: con la pubblicità il primo TITLE_JS (a
        // NavigationCompleted) spesso ritorna vuoto (overlay ad nel DOM) — i tick
        // scout riprovano a ogni secondo finché il titolo non esiste
        let title_sent = std::rc::Rc::new(std::cell::Cell::new(false));
        let scout_tick_events = events.clone();
        let scout_tick_candidates = candidates.clone();
        let _ = webview.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new({
                let scout_deadline = scout_deadline.clone();
                let scout_since = scout_since.clone();
                let ad_notified = ad_notified.clone();
                let title_capture = capture_deadline.clone();
                let title_nav_completed = nav_completed.clone();
                move |_webview, _args| {
                    eprintln!(
                        "[browser] navigazione completata: estraggo titolo + attivo scout (20s)"
                    );
                    title_nav_completed.set(true);
                    let title_events = title_events.clone();
                    let page_thumb = title_page_thumb.clone();
                    let capture_for_title = title_capture.clone();
                    let _ = scout_webview.ExecuteScript(
                        &HSTRING::from(TITLE_JS),
                        &ExecuteScriptCompletedHandler::create(Box::new(
                            move |error_code, json| {
                                error_code?;
                                // ExecuteScript incapsula il ritorno: la stringa JS di
                                // TITLE_JS arriva come JSON string → doppio parse
                                let Ok(payload) = serde_json::from_str::<String>(&json) else {
                                    return Ok(());
                                };
                                // TITLE_JS ritorna {t: titolo, th: thumbnail di pagina}
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                                    match v["t"].as_str() {
                                        Some(title) => {
                                            eprintln!("[browser] titolo: {title}");
                                            let _ = title_events
                                                .send(BrowserEvent::Title(title.to_string()));
                                        }
                                        None => eprintln!(
                                            "[browser] TITLE_JS: campo t assente, payload={:.200}",
                                            payload
                                        ),
                                    }
                                    let thumb = v["th"].as_str().unwrap_or("").trim().to_string();
                                    if !thumb.is_empty() {
                                        *page_thumb.borrow_mut() = Some(thumb.clone());
                                        let _ = title_events.send(BrowserEvent::PageThumb(thumb));
                                    }
                                    // Title/thumb arrivati: se la chiusura post-cattura è
                                    // armata, il timer riparte da qui (il Title non deve
                                    // morire per una corsa con i candidati preload)
                                    if capture_for_title.get().is_some() {
                                        capture_for_title
                                            .set(Some(std::time::Instant::now() + CAPTURE_GRACE));
                                    }
                                }
                                Ok(())
                            },
                        )),
                    );
                    scout_deadline.set(Some(
                        std::time::Instant::now() + std::time::Duration::from_secs(20),
                    ));
                    scout_since.set(Some(std::time::Instant::now()));
                    ad_notified.set(false);
                    Ok(())
                }
            })),
            &mut 0,
        );

        // Sniffing CDP (spec §5). ATTENZIONE ALL'ORDINE: Network.enable deve essere
        // COMPLETATO prima di qualsiasi navigazione — le richieste della pagina
        // (incluso il fetch preload del <video>) altrimenti avvengono prima che il
        // domain sia attivo e gli eventi non arrivano (bug osservato su example:
        // completamento async arrivato dopo la fine del load).
        {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = webview.CallDevToolsProtocolMethod(
                &HSTRING::from("Network.enable"),
                &HSTRING::from("{}"),
                &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
                    move |error_code, _json| {
                        match &error_code {
                            Ok(()) => eprintln!("[browser] Network.enable completato"),
                            Err(e) => eprintln!("[browser] ERRORE Network.enable: {e}"),
                        }
                        let _ = tx.send(error_code);
                        Ok(())
                    },
                )),
            );
            match webview2_com::wait_with_pump(rx) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!("[browser] ERRORE Network.enable (protocollo): {e}");
                    let _ = events.send(BrowserEvent::Error(format!("Network.enable: {e}")));
                }
                Err(e) => {
                    eprintln!("[browser] ERRORE attesa Network.enable: {e}");
                }
            }
        }
        if let Ok(receiver) =
            webview.GetDevToolsProtocolEventReceiver(&HSTRING::from("Network.responseReceived"))
        {
            eprintln!("[browser] receiver Network.responseReceived registrato");
            let cdp_events = events.clone();
            let cdp_candidates = candidates.clone();
            let cdp_page_thumb = page_thumb.clone();
            let cdp_capture = capture_deadline.clone();
            let cdp_ad_active = ad_active_now.clone();
            let cdp_nav_completed = nav_completed.clone();
            let _ = receiver.add_DevToolsProtocolEventReceived(
                &DevToolsProtocolEventReceivedEventHandler::create(Box::new(
                    move |_webview, args| {
                        if let Some(args) = args {
                            let mut json = PWSTR::null();
                            if args.ParameterObjectAsJson(&mut json).is_ok() {
                                let json = take_pwstr(json);
                                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json)
                                {
                                    if verbose_logs()
                                        && let Some(url) = value["response"]["url"]
                                            .as_str()
                                            .map(|u| u.chars().take(140).collect::<String>())
                                    {
                                        eprintln!("[browser] CDP response: {url}");
                                    }
                                    if let Some(mut candidate) =
                                        candidate_from_response_received(&value)
                                    {
                                        // I candidati CDP non hanno contesto DOM: la
                                        // thumbnail di pagina è il fallback migliore
                                        if candidate.thumbnail.is_none()
                                            && let Some(t) = cdp_page_thumb.borrow().clone()
                                        {
                                            candidate.thumbnail = Some(t);
                                        }
                                        if cdp_candidates.borrow_mut().insert(&candidate) {
                                            eprintln!(
                                                "[browser] candidato: {} (stream={})",
                                                candidate.url, candidate.is_stream
                                            );
                                            let _ =
                                                cdp_events.send(BrowserEvent::Candidate(candidate));
                                            // Cattura → 3s di grazia poi chiusura
                                            // (solo dopo NavigationCompleted e fuori ad)
                                            if cdp_nav_completed.get() && !cdp_ad_active.get() {
                                                cdp_capture.set(Some(
                                                    std::time::Instant::now() + CAPTURE_GRACE,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(())
                    },
                )),
                &mut 0,
            );
        } else {
            eprintln!("[browser] ERRORE: GetDevToolsProtocolEventReceiver fallita");
        }

        eprintln!("[browser] navigo about:blank (init completata)");
        let _ = webview.Navigate(&HSTRING::from("about:blank"));

        // Loop comandi + pump messaggi COM (spec §4). I comandi arrivano solo dal
        // canale; i messaggi Windows servono alle async op interne di WebView2.
        loop {
            while let Ok(cmd) = rx.try_recv() {
                // Shutdown gestito qui: termino il processo browser (veloce) così il
                // lock sul profilo cade subito e l'UI può cancellarlo. Il Close()
                // graceful richiede secondi: inutilizzabile per la privacy all'uscita.
                if let BrowserCommand::Shutdown(ack) = cmd {
                    eprintln!("[browser] shutdown: termino il browser (pid={browser_pid})");
                    if browser_pid != 0 {
                        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, browser_pid) {
                            let _ = TerminateProcess(h, 1);
                            let _ = CloseHandle(h);
                        } else {
                            eprintln!("[browser] OpenProcess fallito: procedo con Close()");
                            let _ = controller.Close();
                        }
                    }
                    let _ = ack.send(());
                    return;
                }
                eprintln!("[browser] comando ricevuto");
                exec(
                    &controller,
                    &webview,
                    cmd,
                    &page_thumb,
                    &capture_deadline,
                    &nav_completed,
                    &title_sent,
                );
            }
            // Chiusura post-cattura: il link è in mano all'utente → silenzio totale
            // (TerminateProcess come all'uscita). Il canale si chiude e l'UI
            // resetta lo stato browser (il prossimo "Vai" re-inizializza).
            if let Some(cap) = capture_deadline.get()
                && std::time::Instant::now() >= cap
            {
                capture_deadline.set(None);
                eprintln!("[browser] cattura completa: chiudo il browser (pid={browser_pid})");
                if browser_pid != 0
                    && let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, browser_pid)
                {
                    let _ = TerminateProcess(h, 1);
                    let _ = CloseHandle(h);
                }
                return;
            }
            // Scout DOM a tick (spec §5): ogni 1s fino a 20s dal load, ExecuteScript
            // ISTANTANEO che ritorna i nuovi URL trovati nel DOM live
            if let Some(deadline) = scout_deadline.get() {
                let now = std::time::Instant::now();
                if now >= deadline {
                    scout_deadline.set(None);
                    // Fine finestra scout: chiudi l'eventuale segnalazione ad
                    if ad_notified.replace(false) {
                        let _ = scout_tick_events.send(BrowserEvent::AdWait(false));
                    }
                    eprintln!("[browser] scout: finestra terminata");
                } else if scout_last
                    .get()
                    .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(1))
                {
                    scout_last.set(Some(now));
                    let scout_events = scout_tick_events.clone();
                    let scout_candidates = scout_tick_candidates.clone();
                    let scout_deadline = scout_deadline.clone();
                    let scout_since = scout_since.clone();
                    let ad_notified = ad_notified.clone();
                    let scout_capture = capture_deadline.clone();
                    let scout_ad_active = ad_active_now.clone();
                    let scout_nav_completed = nav_completed.clone();
                    let scout_title_sent = title_sent.clone();
                    let tick_webview = webview.clone();
                    let _ = webview.ExecuteScript(
                        &HSTRING::from(SCOUT_TICK_JS),
                        &ExecuteScriptCompletedHandler::create(Box::new(
                            move |error_code, json| {
                                if let Err(e) = error_code {
                                    eprintln!("[browser] ERRORE scout tick: {e}");
                                    return Ok(());
                                }
                                // SCOUT_TICK_JS ritorna {items: [...], ad_active: bool}
                                let Ok(payload) = serde_json::from_str::<String>(&json) else {
                                    return Ok(());
                                };
                                let Ok(parsed) =
                                    serde_json::from_str::<serde_json::Value>(&payload)
                                else {
                                    return Ok(());
                                };
                                let Ok(items) = serde_json::from_value::<Vec<serde_json::Value>>(
                                    parsed["items"].clone(),
                                ) else {
                                    return Ok(());
                                };
                                for item in &items {
                                    if let Some(url) = item["url"].as_str() {
                                        // Diagnostica: rifiuto del play() (autoplay negato)
                                        if let Some(err) = url.strip_prefix("__play_error__") {
                                            eprintln!("[browser] play() rifiutato: {err}");
                                            continue;
                                        }
                                        // Diagnostica: stato player al primo tick scout
                                        if let Some(d) = url.strip_prefix("__diag__") {
                                            eprintln!("[browser] DIAG {d}");
                                            continue;
                                        }
                                        let from_video =
                                            item["from_video"].as_bool().unwrap_or(false);
                                        let thumbnail = item["thumbnail"].as_str().unwrap_or("");
                                        eprintln!("[browser] DOM scout: {url}");
                                        if let Some(candidate) =
                                            classify_dom(url, from_video, thumbnail)
                                            && scout_candidates.borrow_mut().insert(&candidate)
                                        {
                                            eprintln!(
                                                "[browser] candidato: {} (stream={})",
                                                candidate.url, candidate.is_stream
                                            );
                                            let _ = scout_events
                                                .send(BrowserEvent::Candidate(candidate));
                                        }
                                    }
                                }
                                // Pubblicità attiva: estendi la finestra scout (cap 90s
                                // dalla navigazione) e segnala all'UI il tentativo in
                                // corso. Mentre l'ad gira: niente chiusura post-cattura
                                if parsed["ad_active"].as_bool().unwrap_or(false) {
                                    scout_ad_active.set(true);
                                    scout_capture.set(None);
                                    let within_cap = scout_since.get().is_some_and(|t| {
                                        std::time::Instant::now().duration_since(t)
                                            < std::time::Duration::from_secs(90)
                                    });
                                    if within_cap {
                                        scout_deadline.set(Some(
                                            std::time::Instant::now()
                                                + std::time::Duration::from_secs(20),
                                        ));
                                        if !ad_notified.replace(true) {
                                            eprintln!("[browser] ad active: extended scout window");
                                            let _ = scout_events.send(BrowserEvent::AdWait(true));
                                        }
                                    }
                                } else {
                                    scout_ad_active.set(false);
                                    if ad_notified.replace(false) {
                                        // Ad finita: la finestra resta per il tempo residuo,
                                        // la segnalazione UI si spegne
                                        let _ = scout_events.send(BrowserEvent::AdWait(false));
                                    }
                                    // Tick con nuovi candidati (fuori ad) → (ri)arma la
                                    // chiusura post-cattura
                                    if !items.is_empty() && scout_nav_completed.get() {
                                        scout_capture
                                            .set(Some(std::time::Instant::now() + CAPTURE_GRACE));
                                    }
                                }
                                // Titolo ancora assente (ad in corso): riprova a ogni
                                // tick finché la pagina ne espone uno
                                if !scout_title_sent.get() {
                                    let ev = scout_events.clone();
                                    let sent = scout_title_sent.clone();
                                    let _ =
                                        tick_webview.ExecuteScript(
                                            &HSTRING::from(TITLE_JS),
                                            &ExecuteScriptCompletedHandler::create(Box::new(
                                                move |error_code, json| {
                                                    if error_code.is_err() {
                                                        return Ok(());
                                                    }
                                                    let Ok(payload) =
                                                        serde_json::from_str::<String>(&json)
                                                    else {
                                                        return Ok(());
                                                    };
                                                    let Ok(v) =
                                                        serde_json::from_str::<serde_json::Value>(
                                                            &payload,
                                                        )
                                                    else {
                                                        return Ok(());
                                                    };
                                                    if let Some(title) = v["t"].as_str()
                                                        && !title.is_empty()
                                                    {
                                                        eprintln!(
                                                            "[browser] titolo (tick scout): {title}"
                                                        );
                                                        sent.set(true);
                                                        let _ = ev.send(BrowserEvent::Title(
                                                            title.to_string(),
                                                        ));
                                                    }
                                                    Ok(())
                                                },
                                            )),
                                        );
                                }
                                Ok(())
                            },
                        )),
                    );
                }
            }
            let mut msg = MSG::default();
            if PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            } else {
                let _ = MsgWaitForMultipleObjectsEx(None, 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
        }
    }
}

/// Sposta in fondo allo Z-order le finestre figlie create dal thread browser:
/// la pagina resta "visible" per il player, ma è coperta dalla UI dell'app.
fn push_browser_windows_below(parent_hwnd: isize) {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let list = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
        list.push(hwnd.0 as isize);
        windows::core::BOOL(1)
    }
    let browser_tid = unsafe { GetCurrentThreadId() };
    let mut children: Vec<isize> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(HWND(parent_hwnd as _)),
            Some(collect),
            LPARAM(&mut children as *mut Vec<isize> as isize),
        );
    }
    // Dal fondo della lista verso l'alto: l'ordine relativo tra i figli del
    // browser si conserva, e tutti finiscono sotto le finestre degli altri thread
    for raw in children.iter().rev() {
        let hwnd = HWND(*raw as _);
        let mut tid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut tid)) };
        if tid == browser_tid {
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_BOTTOM),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }
}

fn exec(
    controller: &ICoreWebView2Controller,
    webview: &ICoreWebView2,
    cmd: BrowserCommand,
    page_thumb: &std::rc::Rc<std::cell::RefCell<Option<String>>>,
    capture_deadline: &std::rc::Rc<std::cell::Cell<Option<std::time::Instant>>>,
    nav_completed: &std::rc::Rc<std::cell::Cell<bool>>,
    title_sent: &std::rc::Rc<std::cell::Cell<bool>>,
) {
    unsafe {
        match cmd {
            BrowserCommand::Navigate(url) => {
                // Nuova pagina: la thumbnail di pagina precedente è stantia e
                // la cattura non è ancora avvenuta
                page_thumb.borrow_mut().take();
                capture_deadline.set(None);
                nav_completed.set(false);
                title_sent.set(false);
                let _ = webview.Navigate(&HSTRING::from(url));
            }
            BrowserCommand::Back => {
                page_thumb.borrow_mut().take();
                capture_deadline.set(None);
                nav_completed.set(false);
                title_sent.set(false);
                let _ = webview.GoBack();
            }
            BrowserCommand::Reload => {
                page_thumb.borrow_mut().take();
                capture_deadline.set(None);
                nav_completed.set(false);
                title_sent.set(false);
                let _ = webview.Reload();
            }
            BrowserCommand::SetVisible(visible) => {
                let _ = controller.SetIsVisible(visible);
            }
            BrowserCommand::SetBounds(left, top, right, bottom) => {
                let _ = controller.SetBounds(RECT {
                    left: left as i32,
                    top: top as i32,
                    right: right as i32,
                    bottom: bottom as i32,
                });
            }
            // Gestito nel loop (chiude il thread): irraggiungibile qui
            BrowserCommand::Shutdown(_) => {}
        }
    }
}
