//! Browser embedded: WebView2 su thread dedicato + sniffing CDP. Windows-only.
//!
//! Layering: `sniff.rs` è puro (testabile senza COM); `controller.rs` è l'unico
//! punto che tocca COM/webview2. L'UI parla solo con `BrowserHandle` (comandi) e
//! `BrowserEvent` (canale tokio) — nessun tipo COM attraversa il confine.

pub mod controller;
pub mod sniff;

use std::path::PathBuf;

/// Candidato media individuato dallo sniffing CDP.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaCandidate {
    pub url: String,
    /// true = playlist/manifest (m3u8/mpd): NON scaricabile (aggiornamento futuro)
    pub is_stream: bool,
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
    /// Runtime WebView2 assente o init fallita
    Error(String),
}

/// Comandi UI → thread browser. Tutte le chiamate COM restano sul thread del browser.
/// `Back`/`SetVisible`/`SetBounds`: pronti per "mostra pagina" (fase futura) —
/// il protocollo non cambia quando arriveranno.
#[derive(Clone, Debug)]
pub enum BrowserCommand {
    Navigate(String),
    Back,
    Reload,
    SetVisible(bool),
    /// Rect in px fisici relativi all'area client della finestra padre
    SetBounds(f64, f64, f64, f64),
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
        let _ = self.tx.send(BrowserCommand::SetBounds(left, top, right, bottom));
    }
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
    reqwest::Url::parse(url).ok().and_then(|u| {
        let last = u.path().rsplit('/').next().unwrap_or("");
        let ext = last.rsplit('.').next().unwrap_or("");
        let valid = !ext.is_empty()
            && ext.len() <= 5
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
            && last.contains('.');
        valid.then(|| format!(".{}", ext.to_ascii_lowercase()))
            .filter(|e| e != ".m3u8" && e != ".mpd")
    }).unwrap_or_else(|| ".mp4".to_string())
}

/// Percorso del profilo dedicato del browser embedded (isolato dai cookie utente).
pub fn profile_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("ldwhelper").join("webview-profile")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(sanitize_filename(r#"video: "top" 10/20?"#), "video_ _top_ 10_20_");
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
        assert_eq!(extension_from_url("https://a.com/VID.webm?token=1"), ".webm");
        assert_eq!(extension_from_url("https://a.com/noext"), ".mp4");
        assert_eq!(extension_from_url("https://a.com/x.MP4"), ".mp4");
        // streaming: mai suggestioni m3u8/mpd
        assert_eq!(extension_from_url("https://a.com/index.m3u8"), ".mp4");
        assert_eq!(extension_from_url("https://a.com/index.mpd"), ".mp4");
    }
}
