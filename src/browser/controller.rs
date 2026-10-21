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
    NavigationCompletedEventHandler, NewWindowRequestedEventHandler, PermissionRequestedEventHandler,
    SourceChangedEventHandler, WebMessageReceivedEventHandler, take_pwstr,
};
use windows::{
    core::{HSTRING, Interface, PCWSTR, PWSTR},
    Win32::{
        Foundation::{HWND, RECT},
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
        UI::WindowsAndMessaging::{
            DispatchMessageW, GetClientRect, MSG, MsgWaitForMultipleObjectsEx, PeekMessageW,
            PM_REMOVE, QS_ALLINPUT, MWMO_INPUTAVAILABLE, TranslateMessage,
        },
    },
};

/// Estrazione titolo: chain best-effort (spec §5). ExecuteScript ritorna la
/// stringa JSON-encoded (virgolette incluse): il handler la decodifica.
const TITLE_JS: &str = r#"
(function() {
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
    const h1 = document.querySelector('h1');
    if (h1 && h1.textContent.trim()) return h1.textContent.trim();
    const v = document.querySelector('video');
    if (v) {
        let el = v.previousElementSibling;
        while (el) {
            if (/^(P|H[1-6])$/.test(el.tagName) && el.textContent.trim()) {
                return el.textContent.trim();
            }
            el = el.previousElementSibling;
        }
    }
    return clean(document.title);
})()
"#;

/// Scout DOM: il play forzato NON basta (autoplay spesso negato senza user
/// activation e molti player hanno preload assente → zero fetch → zero eventi
/// CDP). Questo script raccoglie gli URL media DIRETTAMENTE dal DOM live:
/// video src/currentSrc (+ <source>) e link con estensione video, e li invia
/// via postMessage. Include il play muted (player JS che fetchano solo al play).
/// 12 tick × 1s: copre i player che costruiscono il <video> dopo il boot JS.
const DOM_SCOUT_JS: &str = r#"
(function() {
    const seen = {};
    let n = 0;
    const t = setInterval(() => {
        n += 1;
        const out = [];
        document.querySelectorAll('video, video source').forEach(el => {
            let u = el.currentSrc || el.src || el.getAttribute('src') || '';
            if (u && !seen[u]) { seen[u] = 1; out.push({ url: u, from_video: true }); }
        });
        document.querySelectorAll('a[href]').forEach(a => {
            const h = a.getAttribute('href') || '';
            if (/\.(mp4|webm|mkv|mov|avi|m4v|mpg|mpeg|ts)(\?|#|$)/i.test(h) && !seen[h]) {
                seen[h] = 1; out.push({ url: h, from_video: false });
            }
        });
        document.querySelectorAll('video').forEach(v => {
            if (v.paused) { v.muted = true; const p = v.play(); if (p && p.catch) p.catch(() => {}); }
        });
        if (out.length) window.chrome.webview.postMessage(JSON.stringify(out));
        if (n >= 12) clearInterval(t);
    }, 1000);
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
    eprintln!("[browser] thread avviato: hwnd={parent_hwnd}, profilo={}", user_data_dir.display());
    // S_FALSE (già inizializzata) arriva come Ok: solo gli errori veri bloccano
    if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
        eprintln!("[browser] ERRORE: CoInitializeEx fallita");
        let _ = events.send(BrowserEvent::Error("CoInitializeEx: COM non disponibile".into()));
        return;
    }

    let options = CoreWebView2EnvironmentOptions::default();

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
                let _ = events.send(BrowserEvent::Error("WebView2: environment nullo".into()));
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
                tx.send(controller).expect("send controller over mpsc channel");
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
                let _ = events.send(BrowserEvent::Error("WebView2: controller nullo".into()));
                return;
            }
        }
    };

    // Nascosto per sempre (browser in background), ma con bounds = client rect
    // del padre: alcuni siti saltano il caricamento dei media su viewport 0×0.
    let _ = controller.SetIsVisible(false);
    let mut client = RECT::default();
    let _ = GetClientRect(HWND(parent_hwnd as _), &mut client);
    let _ = controller.SetBounds(client);

    let Ok(webview) = controller.CoreWebView2() else {
        eprintln!("[browser] ERRORE: CoreWebView2 assente");
        let _ = events.send(BrowserEvent::Error("WebView2: CoreWebView2 assente".into()));
        return;
    };
    eprintln!("[browser] CoreWebView2 pronto");

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

    // Titolo + autoplay muted dopo ogni navigazione completata (§5)
    let title_events = events.clone();
    let _ = webview.add_NavigationCompleted(
        &NavigationCompletedEventHandler::create(Box::new({
            let title_webview = webview.clone();
            move |_webview, _args| {
                eprintln!("[browser] navigazione completata: estraggo titolo + autoplay");
                let title_events = title_events.clone();
                title_webview.ExecuteScript(
                    &HSTRING::from(TITLE_JS),
                    &ExecuteScriptCompletedHandler::create(Box::new(move |error_code, json| {
                        error_code?;
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json)
                            && let Some(title) = v.as_str()
                        {
                            eprintln!("[browser] titolo: {title}");
                            let _ = title_events.send(BrowserEvent::Title(title.to_string()));
                        }
                        Ok(())
                    })),
                )?;
                let _ = title_webview.ExecuteScript(
                    &HSTRING::from(DOM_SCOUT_JS),
                    &ExecuteScriptCompletedHandler::create(Box::new(|error_code, _json| {
                        error_code
                    })),
                );
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
            &CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |error_code, _json| {
                match &error_code {
                    Ok(()) => eprintln!("[browser] Network.enable completato"),
                    Err(e) => eprintln!("[browser] ERRORE Network.enable: {e}"),
                }
                let _ = tx.send(error_code);
                Ok(())
            })),
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
        // Dedup CONDIVISO tra CDP e scout DOM (possono vedere lo stesso URL)
        let candidates = std::rc::Rc::new(std::cell::RefCell::new(CandidateSet::new(500)));
        let cdp_candidates = candidates.clone();
        let _ = receiver.add_DevToolsProtocolEventReceived(
            &DevToolsProtocolEventReceivedEventHandler::create(Box::new(
                move |_webview, args| {
                    if let Some(args) = args {
                        let mut json = PWSTR::null();
                        if args.ParameterObjectAsJson(&mut json).is_ok() {
                            let json = take_pwstr(json);
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) {
                                if let Some(url) =
                                    value["response"]["url"].as_str().map(|u| {
                                        u.chars().take(140).collect::<String>()
                                    })
                                {
                                    eprintln!("[browser] CDP response: {url}");
                                }
                                if let Some(candidate) =
                                    candidate_from_response_received(&value)
                                    && cdp_candidates.borrow_mut().insert(&candidate)
                                {
                                    eprintln!(
                                        "[browser] candidato: {} (stream={})",
                                        candidate.url, candidate.is_stream
                                    );
                                    let _ = cdp_events.send(BrowserEvent::Candidate(candidate));
                                }
                            }
                        }
                    }
                    Ok(())
                },
            )),
            &mut 0,
        );

        // Scout DOM: i messaggi postati da DOM_SCOUT_JS — src del player anche
        // senza fetch (copre preload assente e autoplay negato)
        let dom_events = events.clone();
        let dom_candidates = candidates.clone();
        let _ = webview.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(move |_webview, args| {
                if let Some(args) = args {
                    let mut json = PWSTR::null();
                    if args.WebMessageAsJson(&mut json).is_ok() {
                        let json = take_pwstr(json);
                        // WebMessageAsJson = stringa JSON-encoded → decodifica
                        if let Ok(payload) = serde_json::from_str::<String>(&json)
                            && let Ok(items) =
                                serde_json::from_str::<Vec<serde_json::Value>>(&payload)
                        {
                            for item in items {
                                if let Some(url) = item["url"].as_str() {
                                    let from_video = item["from_video"].as_bool().unwrap_or(false);
                                    eprintln!("[browser] DOM scout: {url}");
                                    if let Some(candidate) = classify_dom(url, from_video)
                                        && dom_candidates.borrow_mut().insert(&candidate)
                                    {
                                        eprintln!(
                                            "[browser] candidato: {} (stream={})",
                                            candidate.url, candidate.is_stream
                                        );
                                        let _ =
                                            dom_events.send(BrowserEvent::Candidate(candidate));
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(())
            })),
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
            eprintln!("[browser] comando: {cmd:?}");
            exec(&controller, &webview, cmd);
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

fn exec(
    controller: &ICoreWebView2Controller,
    webview: &ICoreWebView2,
    cmd: BrowserCommand,
) {
    unsafe {
        match cmd {
            BrowserCommand::Navigate(url) => {
                let _ = webview.Navigate(&HSTRING::from(url));
            }
            BrowserCommand::Back => {
                let _ = webview.GoBack();
            }
            BrowserCommand::Reload => {
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
        }
    }
}
