//! Parsing PURO degli eventi CDP `Network.responseReceived` → candidati media.
//! Nessuna dipendenza da COM/Windows: testabile con fixture JSON.

use super::MediaCandidate;
use percent_encoding::percent_decode_str;
use std::time::{Duration, Instant};

/// Estrae il candidato dalla params dell'evento CDP `Network.responseReceived`.
///
/// Regole:
/// - URL `http(s)` obbligatorio (`blob:`/`data:` non scaricabili dal motore attuale)
/// - estensione `.m3u8`/`.mpd` o mime `mpegurl`/`dash+xml` → `is_stream = true`
/// - estensione video nota (`.mp4` ecc., anche seguita da `/`) → `is_stream = false`
///   (anche via XHR/Fetch: i player JS scaricano gli mp4 progressivi con range request)
/// - mime `video/*` senza estensione (proxy tipo `remote_control.php?file=x.mp4`)
///   → `is_stream = false`
/// - `resourceType == "Media"` senza estensione → `is_stream = false`
pub fn candidate_from_response_received(params: &serde_json::Value) -> Option<MediaCandidate> {
    let response = params.get("response")?;
    let url = response.get("url")?.as_str()?;
    let mime = response
        .get("mimeType")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let resource_type = params.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let mut candidate = classify(url, mime, resource_type)?;
    // Content-Length dagli header (chiavi case-insensitive, CDP le riporta
    // come inviate dal server)
    candidate.size = response
        .get("headers")
        .and_then(|h| h.as_object())
        .and_then(|h| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        })
        .and_then(|(_, v)| v.as_str())
        .and_then(|v| v.trim().parse::<u64>().ok());
    Some(candidate)
}

/// URL di advertising (pattern forti e conservativi, dominio o segmento path
/// inequivocabile): i media serviti da questi host non diventano candidati.
pub fn is_ad_url(url_lower: &str) -> bool {
    const TOKENS: [&str; 8] = [
        "doubleclick",
        "googlesyndication",
        "adservice",
        "adsystem",
        "adcreative",
        "adserver",
        "/ads/",
        "/ad/",
    ];
    url_lower.contains("advert") || TOKENS.iter().any(|t| url_lower.contains(t))
}

/// Segmenti HLS/DASH (chunk di ~10-60s): inutili da scaricare singolarmente —
/// il manifest li copre e finirebbe in lista un file per chunk.
pub fn is_media_segment(url_lower: &str) -> bool {
    // .m4s esiste SOLO come segmento DASH
    if url_lower.ends_with(".m4s") {
        return true;
    }
    let file = url_lower.split(['?', '#']).next().unwrap_or(url_lower);
    let file = file.rsplit('/').next().unwrap_or("");
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    let stem = stem.to_ascii_lowercase();
    // seg-1-v1-a1.ts / segment12.ts / seg_0007.ts
    (stem.starts_with("seg") || stem.starts_with("chunk"))
        && stem.chars().any(|c| c.is_ascii_digit())
}

pub fn classify(url: &str, mime: &str, resource_type: &str) -> Option<MediaCandidate> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let url_lower = url.to_ascii_lowercase();
    if is_ad_url(&url_lower) || is_media_segment(&url_lower) {
        return None;
    }
    let mime_lower = mime.to_ascii_lowercase();

    if url_lower.ends_with(".m3u8") || mime_lower.contains("mpegurl") {
        return Some(MediaCandidate::new(url, true, false));
    }
    if url_lower.ends_with(".mpd") || mime_lower.contains("dash+xml") {
        return Some(MediaCandidate::new(url, true, false));
    }

    if video_extension(&url_lower) {
        return Some(MediaCandidate::new(url, false, false));
    }
    // Proxy che streamma il video senza estensione nel path (remote_control.php
    // ecc.): il mime video/* lo smaschera anche via XHR/Fetch
    if mime_lower.starts_with("video/") {
        return Some(MediaCandidate::new(url, false, false));
    }
    if resource_type.eq_ignore_ascii_case("Media") {
        return Some(MediaCandidate::new(url, false, false));
    }
    None
}

/// Classifica un URL raccolto dal DOM (scout JS, niente mime né resourceType).
/// `from_video` = proviene da `<video>`/`<source>`: è un media anche SENZA
/// estensione (es. proxy PHP tipo `player-x.php?q=…` che streamma il file).
fn video_extension(url_lower: &str) -> bool {
    let path = url_lower.split(['?', '#']).next().unwrap_or(url_lower);
    // Il path può finire con '/' prima della query (es. /file/token.mp4/?v-acctoken=…)
    let path = path.trim_end_matches('/');
    [
        ".mp4", ".webm", ".mkv", ".mov", ".avi", ".m4v", ".mpg", ".mpeg", ".ts",
    ]
    .iter()
    .any(|ext| path.ends_with(ext))
}

pub fn classify_dom(url: &str, from_video: bool, thumbnail: &str) -> Option<MediaCandidate> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let url_lower = url.to_ascii_lowercase();
    if is_ad_url(&url_lower) || is_media_segment(&url_lower) {
        return None;
    }
    let is_stream = url_lower.ends_with(".m3u8") || url_lower.ends_with(".mpd");
    let mut candidate = if is_stream || from_video || video_extension(&url_lower) {
        Some(MediaCandidate::new(url, is_stream, from_video))
    } else {
        None
    }?;
    candidate.thumbnail = valid_thumbnail(thumbnail);
    Some(candidate)
}

/// Thumbnail accettabile: http(s) e non vuota (il webview non può caricare
/// relativi/blob senza contesto di pagina).
fn valid_thumbnail(url: &str) -> Option<String> {
    let trimmed = url.trim();
    (trimmed.starts_with("http://") || trimmed.starts_with("https://")).then(|| trimmed.to_string())
}

// ---------------------------------------------------------------------------
// Bytestream Meta (Instagram/Facebook): il player MSE scarica video e audio
// come file fMP4 separati, a pezzi con range request firmati (bytestart/
// byteend nella query). La firma CDN copre il range: un URL senza range
// serve SOLO quel pezzo (verificato su fbcdn). Il download quindi raccoglie
// OGNI range visto dal player e li concatena in ordine di bytestart: la
// copertura completa = file completo. Il campo firmato `efg` dice la traccia
// (vencode_tag), l'asset (xpv_asset_id) e su Facebook anche il video_id della
// pagina (per distinguere il reel richiesto dai video del feed).
// ---------------------------------------------------------------------------

/// Provider bytestream noti.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BytestreamProvider {
    #[default]
    Instagram,
    Facebook,
}

impl BytestreamProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Instagram => "Instagram reel",
            Self::Facebook => "Facebook video",
        }
    }
}

/// Un range visto dal player: byte [start..=end] serviti da `url` (firmato
/// per quel range preciso).
#[derive(Debug, Clone, PartialEq)]
pub struct BytestreamRange {
    pub start: u64,
    pub end: u64,
    pub url: String,
}

/// Traccia bytestream classificata.
#[derive(Debug, Clone, PartialEq)]
pub struct BytestreamTrack {
    pub provider: BytestreamProvider,
    /// `xpv_asset_id` dal campo `efg`: identifica il media (video+audio pair)
    pub asset_id: String,
    /// `video_id` dal campo `efg` (solo Facebook): la pagina video di
    /// appartenenza — per filtrare i video del feed
    pub video_id: Option<String>,
    /// true = traccia audio (`vencode_tag` contiene `_audio`)
    pub is_audio: bool,
    pub ranges: Vec<BytestreamRange>,
}

/// Riconosce le richieste bytestream Meta e le classifica. `None` = non e'
/// una richiesta bytestream (prosegui con la classificazione normale).
pub fn classify_bytestream(url: &str) -> Option<BytestreamTrack> {
    let lower = url.to_ascii_lowercase();
    let provider = if lower.contains("cdninstagram.com") {
        BytestreamProvider::Instagram
    } else if lower.contains("fbcdn.net") {
        BytestreamProvider::Facebook
    } else {
        return None;
    };
    if !(lower.contains("bytestart=") || lower.contains("byteend=")) {
        return None;
    }
    let query = url.split_once('?')?.1;
    let encoded = query.split('&').find_map(|p| p.strip_prefix("efg="))?;
    let decoded = percent_decode_str(encoded).decode_utf8().ok()?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(decoded.trim_end())
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(decoded.trim_end()))
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let tag = json.get("vencode_tag")?.as_str()?.to_string();
    let asset_id = json.get("xpv_asset_id")?.to_string();
    let video_id = json
        .get("video_id")
        .and_then(|v| v.as_u64())
        .map(|v| v.to_string());
    let mut start = None;
    let mut end = None;
    for param in query.split('&') {
        if let Some(v) = param.strip_prefix("bytestart=") {
            start = v.parse::<u64>().ok();
        } else if let Some(v) = param.strip_prefix("byteend=") {
            end = v.parse::<u64>().ok();
        }
    }
    let ranges = match (start, end) {
        (Some(start), Some(end)) => vec![BytestreamRange {
            start,
            end,
            url: url.to_string(),
        }],
        _ => return None,
    };
    Some(BytestreamTrack {
        provider,
        asset_id,
        video_id,
        is_audio: tag.contains("_audio"),
        ranges,
    })
}

/// Stato di accoppiamento per un asset bytestream: il player scarica video e
/// audio in tracce separate; il download muxato parte con entrambe.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BytestreamPairing {
    pub provider: BytestreamProvider,
    pub video_ranges: Vec<BytestreamRange>,
    pub video_bitrate: u64,
    pub audio_ranges: Vec<BytestreamRange>,
    pub video_id: Option<String>,
    pub first_seen: Option<Instant>,
    /// Gia' consegnato alla UI: niente secondi flush
    pub flushed: bool,
}

/// Una cattura pronta: ranges video + ranges audio (se separato).
#[derive(Debug, Clone, PartialEq)]
pub struct BytestreamCapture {
    pub provider: BytestreamProvider,
    pub video_ranges: Vec<BytestreamRange>,
    pub audio_ranges: Option<Vec<BytestreamRange>>,
}

/// Sorgente bytestream per il download: i range visti dal player.
#[derive(Debug, Clone, PartialEq)]
pub struct BytestreamSource {
    pub provider: BytestreamProvider,
    pub video_ranges: Vec<BytestreamRange>,
    pub audio_ranges: Option<Vec<BytestreamRange>>,
}

fn merge_ranges(dst: &mut Vec<BytestreamRange>, src: &[BytestreamRange]) {
    for r in src {
        if !dst.iter().any(|d| d.start == r.start && d.end == r.end) {
            dst.push(r.clone());
        }
    }
    dst.sort_by_key(|r| r.start);
}

impl BytestreamPairing {
    /// Aggiorna lo stato con una nuova traccia classificata. Qualita' diverse
    /// condividono lo stesso asset: tieni la traccia col bitrate piu' alto
    /// (e i suoi range).
    pub fn update(&mut self, track: &BytestreamTrack, now: Instant) {
        if self.first_seen.is_none() {
            self.first_seen = Some(now);
            self.provider = track.provider;
        }
        if self.video_id.is_none() {
            self.video_id = track.video_id.clone();
        }
        if track.is_audio {
            merge_ranges(&mut self.audio_ranges, &track.ranges);
            return;
        }
        let new_bitrate = track_bitrate(track.ranges.first().map(|r| r.url.as_str()).unwrap_or(""));
        let replace = match self.video_ranges.is_empty() {
            true => true,
            false => new_bitrate > self.video_bitrate,
        };
        if replace {
            self.video_ranges = track.ranges.clone();
            self.video_bitrate = new_bitrate;
        } else {
            merge_ranges(&mut self.video_ranges, &track.ranges);
        }
    }

    /// Se e' il momento di consegnare. Regole:
    /// - consegna immediata con video+audio entrambi presenti
    /// - con una sola traccia aspetta `hold` (l'altra arriva quasi subito)
    /// - con `focus_video_id` impostato (pagina /reel/<id>) emetti SOLO
    ///   l'asset con quel video_id: gli altri video del feed vengono ignorati
    pub fn ready_to_flush(
        &self,
        now: Instant,
        hold: Duration,
        focus_video_id: Option<&str>,
    ) -> Option<BytestreamCapture> {
        if self.flushed || self.video_ranges.is_empty() {
            return None;
        }
        if let (Some(focus), Some(vid)) = (focus_video_id, &self.video_id)
            && vid != focus
        {
            return None;
        }
        let seen = self.first_seen?;
        let complete = !self.audio_ranges.is_empty();
        let timed_out = now.duration_since(seen) >= hold;
        if !complete && !timed_out {
            return None;
        }
        let audio = (!self.audio_ranges.is_empty()).then(|| self.audio_ranges.clone());
        Some(BytestreamCapture {
            provider: self.provider,
            video_ranges: self.video_ranges.clone(),
            audio_ranges: audio,
        })
    }
}

/// Estrae il bitrate dal query param `efg` (base64url JSON): 0 se non leggibile.
fn track_bitrate(url: &str) -> u64 {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    let Some(encoded) = query.split('&').find_map(|p| p.strip_prefix("efg=")) else {
        return 0;
    };
    let decoded = match percent_decode_str(encoded).decode_utf8() {
        Ok(d) => d,
        Err(_) => return 0,
    };
    use base64::Engine as _;
    let bytes = match base64::engine::general_purpose::URL_SAFE
        .decode(decoded.trim_end())
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(decoded.trim_end()))
    {
        Ok(b) => b,
        Err(_) => return 0,
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|j| j.get("bitrate")?.as_u64())
        .unwrap_or(0)
}

/// L'id del video di una pagina reel/watch, se l'URL lo espone
/// (`/reel/<id>`, `/videos/<id>`, `/watch/?v=<id>`).
pub fn focus_video_id_from_url(page_url: &str) -> Option<String> {
    let lower = page_url.to_ascii_lowercase();
    for marker in ["/reel/", "/videos/", "/watch/?v=", "&v="] {
        if let Some(pos) = lower.find(marker) {
            let rest = &page_url[pos + marker.len()..];
            let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// Raccolta dedup con cap: il thread browser inoltra all'UI solo i candidati nuovi.
/// Ordine di inserimento (l'UI mostra newest-first prependendo).
pub struct CandidateSet {
    seen: std::collections::HashSet<String>,
    count: usize,
    cap: usize,
}

impl CandidateSet {
    pub fn new(cap: usize) -> Self {
        Self {
            seen: Default::default(),
            count: 0,
            cap,
        }
    }

    /// `true` se il candidato è nuovo e sotto cap (va inoltrato all'UI).
    pub fn insert(&mut self, c: &MediaCandidate) -> bool {
        if self.count >= self.cap {
            return false;
        }
        if self.seen.insert(c.url.clone()) {
            self.count += 1;
            return true;
        }
        false
    }

    #[allow(dead_code)] // utile nei test/diagnostica
    pub fn len(&self) -> usize {
        self.count
    }

    #[allow(dead_code)] // utile nei test/diagnostica
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(url: &str, mime: &str, r#type: &str) -> serde_json::Value {
        json!({
            "type": r#type,
            "requestId": "1",
            "response": { "url": url, "mimeType": mime, "status": 200 }
        })
    }

    #[test]
    fn media_resource_is_candidate() {
        let c = candidate_from_response_received(&params(
            "https://cdn.com/v/clip.mp4",
            "video/mp4",
            "Media",
        ))
        .expect("candidato atteso");
        assert!(!c.is_stream);
        assert_eq!(c.url, "https://cdn.com/v/clip.mp4");
    }

    #[test]
    fn mp4_via_xhr_is_not_stream() {
        // i player JS scaricano mp4 progressivi via XHR/Fetch con range request
        let c = candidate_from_response_received(&params(
            "https://cdn.com/v/movie.mp4",
            "application/octet-stream",
            "XHR",
        ))
        .expect("candidato atteso");
        assert!(!c.is_stream);
    }

    #[test]
    fn m3u8_by_extension_and_by_mime() {
        let c = candidate_from_response_received(&params(
            "https://cdn.com/hls/index.m3u8",
            "application/vnd.apple.mpegurl",
            "XHR",
        ))
        .expect("candidato atteso");
        assert!(c.is_stream);

        // playlist servita senza estensione: il mimeType la smaschera
        let c = candidate_from_response_received(&params(
            "https://cdn.com/hls/playlist",
            "application/vnd.apple.mpegurl",
            "Fetch",
        ))
        .expect("candidato atteso");
        assert!(c.is_stream);
    }

    #[test]
    fn mpd_by_extension_and_by_mime() {
        let c = candidate_from_response_received(&params(
            "https://cdn.com/dash/manifest.mpd",
            "application/dash+xml",
            "Fetch",
        ))
        .expect("candidato atteso");
        assert!(c.is_stream);
    }

    #[test]
    fn ignores_non_media_resources() {
        assert!(
            candidate_from_response_received(&params(
                "https://cdn.com/app.js",
                "application/javascript",
                "Script",
            ))
            .is_none()
        );
        assert!(
            candidate_from_response_received(&params(
                "https://cdn.com/img.png",
                "image/png",
                "Image",
            ))
            .is_none()
        );
    }

    #[test]
    fn ignores_blob_and_data_urls() {
        assert!(
            candidate_from_response_received(&params(
                "blob:https://cdn.com/1234",
                "video/mp4",
                "Media",
            ))
            .is_none()
        );
        assert!(
            candidate_from_response_received(&params(
                "data:video/mp4;base64,AAAA",
                "video/mp4",
                "Media",
            ))
            .is_none()
        );
    }

    #[test]
    fn malformed_params_return_none() {
        assert!(candidate_from_response_received(&json!({})).is_none());
        assert!(candidate_from_response_received(&json!({ "response": {} })).is_none());
    }

    #[test]
    fn candidate_set_dedups_and_caps() {
        let mut set = CandidateSet::new(2);
        let a = MediaCandidate::new("https://a.com/1", false, false);
        let b = MediaCandidate::new("https://a.com/2", true, false);

        assert!(set.insert(&a));
        assert!(!set.insert(&a), "dedup per URL");
        assert!(set.insert(&b));
        assert_eq!(set.len(), 2);

        let c = MediaCandidate::new("https://a.com/3", false, false);
        assert!(!set.insert(&c), "cap raggiunto: inoltro bloccato");
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn content_length_from_headers_case_insensitive() {
        let params = json!({
            "type": "Media",
            "response": {
                "url": "https://cdn.com/v/movie.mp4",
                "mimeType": "video/mp4",
                "headers": { "Content-Length": "1048576" }
            }
        });
        let c = candidate_from_response_received(&params).expect("atteso");
        assert_eq!(c.size, Some(1_048_576));
        assert_eq!(c.name, "movie.mp4");

        let params = json!({
            "type": "Media",
            "response": {
                "url": "https://cdn.com/v/movie2.mp4",
                "headers": { "content-length": "42" }
            }
        });
        let c = candidate_from_response_received(&params).expect("atteso");
        assert_eq!(c.size, Some(42));
    }

    #[test]
    fn dom_video_src_without_extension_is_candidate() {
        // proxy PHP che streamma il video: nessuna estensione ma è src del <video>
        let c = classify_dom("https://sito.com/player-x.php?q=cG9zdF9pZA%3D%3D", true, "")
            .expect("candidato atteso");
        assert!(!c.is_stream);
        assert!(c.thumbnail.is_none());
    }

    #[test]
    fn dom_link_only_with_video_extension() {
        let c = classify_dom("https://sito.com/clip.mp4?token=1", false, "").expect("atteso");
        assert!(!c.is_stream);
        assert!(classify_dom("https://sito.com/pagina.html", false, "").is_none());
        assert!(classify_dom("https://sito.com/thumb.jpg", false, "").is_none());
    }

    #[test]
    fn dom_thumbnail_propagated_and_validated() {
        let c = classify_dom(
            "https://sito.com/clip.mp4",
            true,
            "https://sito.com/poster.jpg",
        )
        .expect("atteso");
        assert_eq!(c.thumbnail.as_deref(), Some("https://sito.com/poster.jpg"));
        // relativo/blob/vuoto → scartati
        for bad in ["", "/relativo.jpg", "blob:https://sito.com/1"] {
            let c = classify_dom("https://sito.com/clip.mp4", true, bad).expect("atteso");
            assert!(
                c.thumbnail.is_none(),
                "thumbnail {bad:?} deve essere scartata"
            );
        }
    }

    #[test]
    fn mp4_path_with_trailing_slash_is_candidate() {
        // KVS: /file/<token>.mp4/?v-acctoken=... — lo slash PRIMA della
        // query non deve nascondere l'estensione
        let c = classify(
            "https://www.sito.com/file/aB3cdE.mp4/?v-acctoken=xyz&rnd=1",
            "text/html", // hop 302: mime del redirect, non del video
            "Media",
        )
        .expect("atteso");
        assert!(!c.is_stream);
        // l'estensione basta anche senza resourceType Media
        assert!(classify("https://www.sito.com/file/aB3cdE.mp4/", "", "Document").is_some());
        // currentSrc del <video> con la stessa forma
        assert!(classify_dom("https://sito.com/file/aB3cdE.mp4/?t=1", false, "").is_some());
    }

    #[test]
    fn video_mime_without_extension_is_candidate() {
        // proxy PHP che streamma il video: estensione .mp4 solo nel VALORE
        // della query, path termina con .php → decide il mime video/*
        let c = classify(
            "https://dl.host.com/remote_control.php?file=abc-xyz.mp4&acctoken=xyz",
            "video/mp4",
            "XHR",
        )
        .expect("atteso");
        assert!(!c.is_stream);
        // octet-stream senza estensione resta fuori (comportamento invariato)
        assert!(
            classify(
                "https://sito.com/bin?id=1",
                "application/octet-stream",
                "XHR"
            )
            .is_none()
        );
    }

    #[test]
    fn media_segments_are_filtered() {
        assert!(classify("https://cdn.com/hls/seg-1-v1-a1.ts", "video/mp2t", "Media").is_none());
        assert!(
            classify(
                "https://cdn.com/dash/index_3.m4s",
                "video/iso.segment",
                "Media"
            )
            .is_none()
        );
        assert!(classify_dom("https://cdn.com/hls/segment12.ts", false, "").is_none());
        assert!(classify_dom("https://cdn.com/hls/chunk_0007.ts", false, "").is_none());
        // .ts generico (non segmento) e tutto il resto passano
        assert!(classify_dom("https://sito.com/movie.ts", false, "").is_some());
        assert!(classify_dom("https://sito.com/clip.mp4", false, "").is_some());
    }

    #[test]
    fn ad_urls_are_never_candidates() {
        // CDP e DOM: gli host/segmenti advertising pubblici sono scartati
        assert!(
            classify(
                "https://pubads.g.doubleclick.net/v.mp4",
                "video/mp4",
                "Media"
            )
            .is_none()
        );
        assert!(classify("https://cdn.com/ads/spot.mp4", "video/mp4", "Media").is_none());
        assert!(classify_dom("https://sito.com/ad/clip.mp4", true, "").is_none());
        assert!(classify_dom("https://googlesyndication.com/x.m3u8", true, "").is_none());
        // falsi positivi da evitare: "/add/", "/read/", "upload" passano
        assert!(classify_dom("https://sito.com/add/clip.mp4", true, "").is_some());
        assert!(classify_dom("https://sito.com/read/clip.mp4", false, "").is_some());
        assert!(classify_dom("https://sito.com/upload/clip.mp4", false, "").is_some());
    }

    #[test]
    fn dom_m3u8_is_stream() {
        let c = classify_dom("https://sito.com/hls/index.m3u8", true, "").expect("atteso");
        assert!(c.is_stream);
        // blob: (MSE) non scaricabile → scartato, lo prende il CDP
        assert!(classify_dom("blob:https://sito.com/1234", true, "").is_none());
    }

    // URL reali dal traffico Instagram (reel pubblico, MSE bytestream)
    const EFG_VIDEO: &str = "eyJ2ZW5jb2RlX3RhZyI6ImlnLXhwdmRzLmNsaXBzLmlnd3d3LUMzLmRhc2hfYmFzZWxpbmVfMV92MSIsInZpZGVvX2lkIjpudWxsLCJvaWxfdXJsZ2VuX2FwcF9pZCI6OTM2NjE5NzQzMzkyNDU5LCJjbGllbnRfbmFtZSI6ImlnIiwieHB2X2Fzc2V0X2lkIjoxODUzMTE4MjkyMDA2OTQwNSwiYXNzZXRfYWdlX2RheXMiOjIzMSwidmlfdXNlY2FzZV9pZCI6MTA4MjcsImR1cmF0aW9uX3MiOjE4LCJiaXRyYXRlIjoxMTA4NjkyLCJ1cmxnZW5fc291cmNlIjoid3d3In0%3D";
    const EFG_AUDIO: &str = "eyJ2ZW5jb2RlX3RhZyI6ImlnLXhwdmRzLmNsaXBzLmlnd3d3LUMzLmRhc2hfbG5faGVhYWNfdmJyM19hdWRpbyIsInZpZGVvX2lkIjpudWxsLCJvaWxfdXJsZ2VuX2FwcF9pZCI6OTM2NjE5NzQzMzkyNDU5LCJjbGllbnRfbmFtZSI6ImlnIiwieHB2X2Fzc2V0X2lkIjoxODUzMTE4MjkyMDA2OTQwNSwiYXNzZXRfYWdlX2RheXMiOjIzMSwidmlfdXNlY2FzZV9pZCI6MTA4MjcsImR1cmF0aW9uX3MiOjE4LCJiaXRyYXRlIjo0NzEzOSwidXJsZ2VuX3NvdXJjZSI6Ind3dyJ9";
    // efg Facebook (dal traffico reale): contiene il video_id della pagina
    const EFG_FB_VIDEO: &str = "eyJ2ZW5jb2RlX3RhZyI6ImRhc2hfcjJhdjEtcjFnZW4ydnA5LW0zX3E3MCIsInZpZGVvX2lkIjoxNTg2Nzk1Mjk2MDgzMDA0LCJvaWxfdXJsZ2VuX2FwcF9pZCI6MCwiY2xpZW50X25hbWUiOiJ1bmtub3duIiwieHB2X2Fzc2V0X2lkIjoxNDIzNzE1NTQ5ODc4MzYwLCJhc3NldF9hZ2VfZGF5cyI6MCwidmlfdXNlY2FzZV9pZCI6MTAxMjIsImR1cmF0aW9uX3MiOjE5LCJiaXRyYXRlIjoxMzcyMTMsInVybGdlbl9zb3VyY2UiOiJ3d3cifQ%3D%3D";

    #[test]
    fn classify_bytestream_instagram_video() {
        let url = format!(
            "https://scontent.cdninstagram.com/o1/v/clip.mp4?efg={EFG_VIDEO}&bytestart=0&byteend=891"
        );
        let track = classify_bytestream(&url).expect("traccia instagram attesa");
        assert_eq!(track.provider, BytestreamProvider::Instagram);
        assert_eq!(track.asset_id, "18531182920069405");
        assert!(!track.is_audio);
        assert_eq!(track.video_id, None);
        // un solo range, con i byte visti
        assert_eq!(track.ranges.len(), 1);
        assert_eq!((track.ranges[0].start, track.ranges[0].end), (0, 891));
        assert!(track.ranges[0].url.contains("bytestart=0"));
    }

    #[test]
    fn classify_bytestream_facebook_exposes_video_id() {
        let url = format!(
            "https://video-dus1-1.xx.fbcdn.net/o1/v/t2/clip.mp4?efg={EFG_FB_VIDEO}&bytestart=80455&byteend=421200"
        );
        let track = classify_bytestream(&url).expect("traccia facebook attesa");
        assert_eq!(track.provider, BytestreamProvider::Facebook);
        assert_eq!(track.video_id.as_deref(), Some("1586795296083004"));
        assert_eq!(track.asset_id, "1423715549878360");
        assert_eq!(
            (track.ranges[0].start, track.ranges[0].end),
            (80455, 421200)
        );
    }

    #[test]
    fn classify_bytestream_instagram_audio() {
        let url = format!(
            "https://scontent.cdninstagram.com/a/aud.mp4?efg={EFG_AUDIO}&bytestart=0&byteend=975"
        );
        let track = classify_bytestream(&url).expect("traccia audio attesa");
        assert!(track.is_audio);
        assert_eq!(track.asset_id, "18531182920069405");
        assert_eq!(track.provider, BytestreamProvider::Instagram);
    }

    #[test]
    fn classify_bytestream_rejects_non_meta() {
        assert!(
            classify_bytestream("https://cdn.example.com/v/clip.mp4?bytestart=0&byteend=9")
                .is_none()
        );
        // richieste fbcdn senza range: JS/CSS statici → non bytestream
        assert!(classify_bytestream("https://static.xx.fbcdn.net/rsrc.php/v4/yS/r/x.js").is_none());
        assert!(classify_bytestream("https://cdninstagram.com/v/clip.mp4").is_none());
        // range senza efg: non classificabile
        assert!(
            classify_bytestream("https://cdninstagram.com/v/clip.mp4?bytestart=0&byteend=9")
                .is_none()
        );
    }

    #[test]
    fn pairing_ranges_merge_and_sort() {
        let now = Instant::now();
        let mut pair = BytestreamPairing::default();
        let url_v =
            format!("https://video.fbcdn.net/clip.mp4?efg={EFG_FB_VIDEO}&bytestart=0&byteend=906",);
        let t1 = classify_bytestream(&url_v).expect("traccia");
        pair.update(&t1, now);
        // stesso range ripetuto: dedup
        pair.update(&t1, now + Duration::from_secs(1));
        assert_eq!(pair.video_ranges.len(), 1);
        // range successivo dello stesso asset: aggiunto e ordinato
        let url_v2 = format!(
            "https://video.fbcdn.net/clip.mp4?efg={EFG_FB_VIDEO}&bytestart=907&byteend=80454",
        );
        let t2 = classify_bytestream(&url_v2).expect("traccia 2");
        pair.update(&t2, now + Duration::from_secs(2));
        assert_eq!(pair.video_ranges.len(), 2);
        assert!(
            pair.video_ranges[0].start < pair.video_ranges[1].start,
            "range ordinati per bytestart"
        );
    }

    #[test]
    fn pairing_facebook_focus_selects_the_reel() {
        let now = Instant::now();
        let mut pair = BytestreamPairing::default();
        let url_reel =
            format!("https://video.fbcdn.net/clip.mp4?efg={EFG_FB_VIDEO}&bytestart=0&byteend=906");
        pair.update(&classify_bytestream(&url_reel).unwrap(), now);
        // video + audio completi: pronti
        assert!(pair.audio_ranges.is_empty());
        let audio_url = "https://video.fbcdn.net/aud.mp4?bytestart=0&byteend=97";
        pair.audio_ranges = vec![BytestreamRange {
            start: 0,
            end: 97,
            url: audio_url.into(),
        }];
        // con focus sul reel: emesso
        let cap = pair
            .ready_to_flush(now, Duration::from_secs(6), Some("1586795296083004"))
            .expect("reel atteso");
        assert_eq!(cap.provider, BytestreamProvider::Facebook);
        assert_eq!(cap.video_ranges.len(), 1);
        assert_eq!(cap.audio_ranges.as_ref().map(|a| a.len()), Some(1));
        // con focus su un ALTRO video: ignorato
        let pair2 = pair.clone();
        assert!(
            pair2
                .ready_to_flush(now, Duration::from_secs(6), Some("999"))
                .is_none()
        );
        // senza focus: emesso comunque
        assert!(
            pair2
                .ready_to_flush(now, Duration::from_secs(6), None)
                .is_some()
        );
    }

    #[test]
    fn pairing_instagram_video_only_flushes_after_hold() {
        let now = Instant::now();
        let mut pair = BytestreamPairing::default();
        let url_v = format!(
            "https://scontent.cdninstagram.com/v/clip.mp4?efg={EFG_VIDEO}&bytestart=0&byteend=891"
        );
        pair.update(&classify_bytestream(&url_v).unwrap(), now);
        // hold non scaduta: niente
        assert!(
            pair.ready_to_flush(now, Duration::from_secs(6), None)
                .is_none()
        );
        // scaduta: flush della sola traccia video
        let cap = pair
            .ready_to_flush(now + Duration::from_secs(7), Duration::from_secs(6), None)
            .expect("flush video atteso");
        assert!(cap.audio_ranges.is_none());
    }
}
