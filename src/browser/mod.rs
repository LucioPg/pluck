//! Browser embedded: WebView2 su thread dedicato + sniffing CDP. Windows-only.
//!
//! Layering: `sniff.rs` è puro (testabile senza COM); `controller.rs` è l'unico
//! punto che tocca COM/webview2. L'UI parla solo con `BrowserHandle` (comandi) e
//! `BrowserEvent` (canale tokio) — nessun tipo COM attraversa il confine.

pub mod controller;
pub mod sniff;

use std::path::PathBuf;

/// Candidato media individuato dallo sniffing (CDP o scout DOM).
#[derive(Clone, Debug, PartialEq)]
pub struct MediaCandidate {
    pub url: String,
    /// true = playlist/manifest (m3u8/mpd): NON scaricabile (aggiornamento futuro)
    pub is_stream: bool,
    /// Nome leggibile: ultimo segmento del path decodificato (fallback: host)
    pub name: String,
    /// Dimensione in byte se nota (Content-Length da CDP o probe HEAD)
    pub size: Option<u64>,
    /// Thumbnail: poster del `<video>`/img dell'anchor (scout) o thumb di
    /// pagina (og:image, fallback per i candidati CDP)
    pub thumbnail: Option<String>,
}

impl MediaCandidate {
    pub fn new(url: impl Into<String>, is_stream: bool, from_video: bool) -> Self {
        let url = url.into();
        Self {
            name: name_from_url(&url, from_video),
            size: None,
            is_stream,
            url,
            thumbnail: None,
        }
    }
}

/// Nome leggibile dall'URL: ultimo segmento del path (decodificato). Se il
/// segmento è un endpoint tecnico (`.php`, `.aspx`, vuoto) → host + query corta.
pub fn name_from_url(url: &str, _from_video: bool) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return url.to_string();
    };
    let raw = parsed
        .path()
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("");
    // Decodifica percentuale (%XX); il '+' nel path è letterale (non form-encoding)
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .to_string();
    let technical = decoded.is_empty()
        || decoded.to_ascii_lowercase().ends_with(".php")
        || decoded.to_ascii_lowercase().ends_with(".aspx")
        || decoded.to_ascii_lowercase().ends_with(".jsp");
    if technical {
        // endpoint proxy: usa l'host come nome riconoscibile
        let host = parsed.host_str().unwrap_or("media");
        format!("{host}/{decoded}")
    } else {
        decoded
    }
}

/// Origin del sito da un URL: `https://www.sito.it/media/file.mp4` →
/// `https://www.sito.it/`. Referer per i download automatici (la root è ciò che
/// i CDN tipicamente accettano; il path completo no). URL non valido → invariato.
pub fn site_root(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|u| match u.port() {
            Some(port) => format!("{}://{}:{}/", u.scheme(), u.host_str().unwrap_or(""), port),
            None => format!("{}://{}/", u.scheme(), u.host_str().unwrap_or("")),
        })
        .unwrap_or_else(|_| url.to_string())
}

/// HEAD probe: dimensione del candidato DOM (senza Content-Length da CDP).
/// Redirect seguiti (URL firmati CDN); 3s di timeout; None = non determinabile.
pub async fn probe_size(url: &str, referer: Option<&str>) -> Option<u64> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let mut req = client.head(url);
    if let Some(r) = referer {
        req = req.header("Referer", r);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Formatta byte in unità leggibili (base 1024): `942 B`, `1.5 MB`, `2.0 GB`…
pub fn format_bytes(v: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut val = v as f64;
    let mut unit = 0usize;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{v} B")
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

/// Eventi dal thread browser verso l'UI (canale tokio, `blocking_send` lato thread).
#[derive(Clone, Debug)]
pub enum BrowserEvent {
    /// Nuovo candidato media (già dedup lato thread browser)
    Candidate(MediaCandidate),
    /// URL della pagina corrente (per referer)
    PageUrl(String),
    /// Titolo estratto dal DOM live (chain og:title → h1 → title)
    Title(String),
    /// Thumbnail di pagina (og:image/twitter:image/poster): l'UI riempie i
    /// candidati ancora senza thumbnail. Stringa vuota = nessuna trovata.
    PageThumb(String),
    /// Rilevata pubblicità: finestra scout estesa (true) o terminata (false).
    /// L'UI lo mostra accanto allo spinner come feedback dei tentativi.
    AdWait(bool),
    /// Runtime WebView2 assente o init fallita
    Error(String),
}

/// Comandi UI → thread browser. Tutte le chiamate COM restano sul thread del browser.
/// `Back`/`SetVisible`/`SetBounds`: pronti per "mostra pagina" (fase futura) —
/// il protocollo non cambia quando arriveranno.
pub enum BrowserCommand {
    Navigate(String),
    Back,
    Reload,
    SetVisible(bool),
    /// Rect in px fisici relativi all'area client della finestra padre
    SetBounds(f64, f64, f64, f64),
    /// Chiusura: Close() del controller + ack (rilascia il lock sul profilo)
    Shutdown(std::sync::mpsc::Sender<()>),
}

/// Handle per comandare il browser. I metodi ignorano l'errore di send: se il
/// thread è morto (init fallita) l'UI mostra già il banner `BrowserEvent::Error`.
#[derive(Clone)]
pub struct BrowserHandle {
    tx: std::sync::mpsc::Sender<BrowserCommand>,
}

impl BrowserHandle {
    pub fn navigate(&self, url: &str) {
        let _ = self.tx.send(BrowserCommand::Navigate(url.to_string()));
    }

    #[allow(dead_code)] // fase futura: "mostra pagina" per login
    pub fn back(&self) {
        let _ = self.tx.send(BrowserCommand::Back);
    }

    pub fn reload(&self) {
        let _ = self.tx.send(BrowserCommand::Reload);
    }

    #[allow(dead_code)] // fase futura: "mostra pagina" per login
    pub fn set_visible(&self, visible: bool) {
        let _ = self.tx.send(BrowserCommand::SetVisible(visible));
    }

    #[allow(dead_code)] // fase futura: "mostra pagina" per login
    pub fn set_bounds(&self, left: f64, top: f64, right: f64, bottom: f64) {
        let _ = self
            .tx
            .send(BrowserCommand::SetBounds(left, top, right, bottom));
    }

    /// Shutdown graceful: Close() del controller + attesa ack (max `timeout`).
    /// `true` = ack ricevuto → il lock sul profilo è rilasciato.
    pub fn shutdown_and_wait(&self, timeout: std::time::Duration) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.tx.send(BrowserCommand::Shutdown(tx)).is_err() {
            return false; // thread già morto: lock comunque rilasciato
        }
        rx.recv_timeout(timeout).is_ok()
    }
}

/// Handle condiviso per lo shutdown all'uscita (impostato da init_browser).
static SHARED_BROWSER: std::sync::OnceLock<BrowserHandle> = std::sync::OnceLock::new();

pub fn register_shared_browser(handle: &BrowserHandle) {
    let _ = SHARED_BROWSER.set(handle.clone());
}

pub fn shared_browser() -> Option<&'static BrowserHandle> {
    SHARED_BROWSER.get()
}

/// Filename suggerito: il titolo del video pulito per Windows.
/// Caratteri illegali `\ / : * ? " < > |` → `_`; trim di spazi/punti finali;
/// cap a 120 caratteri; vuoto → fallback "video".
pub fn sanitize_filename(raw: &str) -> String {
    const ILLEGAL: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
    let mut out: String = raw
        .chars()
        .map(|c| if ILLEGAL.contains(&c) { '_' } else { c })
        .collect();
    // Collassa spazi multipli e taglia i controlli
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    if out.chars().count() > 120 {
        out = out.chars().take(120).collect();
        while out.ends_with('.') || out.ends_with(' ') {
            out.pop();
        }
    }
    // Vuoto o fatto solo di separatori ("___") → nessuna informazione utile
    if !out.chars().any(|c| c.is_alphanumeric()) {
        return "video".to_string();
    }
    out
}

/// Estensione dal path dell'URL del candidato (default `.mp4`).
pub fn extension_from_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            let last = u.path().rsplit('/').next().unwrap_or("");
            let ext = last.rsplit('.').next().unwrap_or("");
            let valid = !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && last.contains('.');
            valid
                .then(|| format!(".{}", ext.to_ascii_lowercase()))
                .filter(|e| e != ".m3u8" && e != ".mpd")
        })
        .unwrap_or_else(|| ".mp4".to_string())
}

/// Percorso del profilo dedicato del browser embedded (isolato dai cookie utente).
/// `cache_dir` = %LOCALAPPDATA% su Windows: dati effimeri cancellati a ogni
/// uscita — mai in Roaming (che è per impostazioni che migrano tra macchine).
pub fn profile_dir() -> PathBuf {
    let base = dirs::cache_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("ldwhelper").join("webview-profile")
}

/// Shutdown del browser condiviso + cancellazione del profilo WebView2
/// (rilascio lock profilo, cookie/login/cache non persistono su disco).
/// Sincrono: da chiamare sull'uscita effettiva (menu tray "Esci") prima di
/// `std::process::exit`. Retry: il processo browser può rilasciare i file
/// qualche centinaio di ms dopo il Close().
pub fn shutdown_and_cleanup_profile() {
    if let Some(handle) = shared_browser()
        && !handle.shutdown_and_wait(std::time::Duration::from_secs(3))
    {
        eprintln!("[exit] ack shutdown non arrivato (timeout): procedo comunque");
    }
    let profile = profile_dir();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(
            sanitize_filename(r#"video: "top" 10/20?"#),
            "video_ _top_ 10_20_"
        );
    }

    #[test]
    fn sanitize_collapses_whitespace_and_trims() {
        assert_eq!(sanitize_filename("  titolo   doppio  "), "titolo doppio");
        assert_eq!(sanitize_filename("punto finale. "), "punto finale");
    }

    #[test]
    fn sanitize_caps_length_and_empty_fallback() {
        let long = "x".repeat(300);
        assert_eq!(sanitize_filename(&long).chars().count(), 120);
        assert_eq!(sanitize_filename("???"), "video");
        assert_eq!(sanitize_filename(""), "video");
    }

    #[test]
    fn extension_from_url_paths() {
        assert_eq!(extension_from_url("https://a.com/v/clip.mp4"), ".mp4");
        assert_eq!(
            extension_from_url("https://a.com/VID.webm?token=1"),
            ".webm"
        );
        assert_eq!(extension_from_url("https://a.com/noext"), ".mp4");
        assert_eq!(extension_from_url("https://a.com/x.MP4"), ".mp4");
        // streaming: mai suggestioni m3u8/mpd
        assert_eq!(extension_from_url("https://a.com/index.m3u8"), ".mp4");
        assert_eq!(extension_from_url("https://a.com/index.mpd"), ".mp4");
    }

    #[test]
    fn name_from_url_segments() {
        assert_eq!(
            name_from_url("https://cdn.com/media/2026/10/il%20mio%20video.mp4", true),
            "il mio video.mp4"
        );
        assert_eq!(
            name_from_url("https://a.com/v/clip.mp4?md5=x&expires=1", false),
            "clip.mp4"
        );
    }

    #[test]
    fn name_from_url_proxy_falls_back_to_host() {
        let n = name_from_url(
            "https://example.com/wp-content/plugins/player-x.php?q=abc",
            true,
        );
        assert_eq!(n, "example.com/player-x.php");
        // path vuoto (solo dominio)
        assert_eq!(name_from_url("https://cdn.com/", true), "cdn.com/");
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(942), "942 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(2 * 1024_u64.pow(3)), "2.0 GB");
        let big = 8 * 1024_u64.pow(4);
        assert!(format_bytes(big).ends_with("TB"));
    }

    #[test]
    fn site_root_keeps_scheme_and_host() {
        assert_eq!(
            site_root("https://www.sito.it/media/file.mp4"),
            "https://www.sito.it/"
        );
        assert_eq!(
            site_root("http://cdn.example.com/a/b.mp4?token=1"),
            "http://cdn.example.com/"
        );
        assert_eq!(site_root("https://host:8443/x.m3u8"), "https://host:8443/");
        // URL non valido → invariato (chiamanti filtrano il vuoto)
        assert_eq!(site_root(""), "");
    }
}
