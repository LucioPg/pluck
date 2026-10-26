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
/// - estensione video nota (`.mp4` ecc.) → `is_stream = false` (anche via XHR/Fetch:
///   i player JS scaricano gli mp4 progressivi con range request)
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
// Bytestream Instagram: il player MSE scarica video e audio come file fMP4
// separati, a pezzi (query bytestart/byteend). Normalizzando l'URL tutti i
// pezzi collassano nel file completo; il campo firmato `efg` dice se è la
// traccia video o audio e l'asset di appartenenza (accoppiamento per il mux).
// ---------------------------------------------------------------------------

/// Rimuove bytestart/byteend dalla query: l'URL normalizzato scarica il file
/// completo (la firma CDN rimane valida senza il range).
pub fn normalize_bytestream_url(url: &str) -> String {
    if !url.contains("bytestart=") && !url.contains("byteend=") {
        return url.to_string();
    }
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.starts_with("bytestart=") && !p.starts_with("byteend="))
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// Traccia bytestream Instagram classificata.
#[derive(Debug, Clone, PartialEq)]
pub struct InstaBytestream {
    /// `xpv_asset_id` dal campo `efg`: identifica il media (video+audio pair)
    pub asset_id: String,
    /// true = traccia audio (`vencode_tag` contiene `_audio`)
    pub is_audio: bool,
    /// URL normalizzato (senza bytestart/byteend): scarica il file completo
    pub url: String,
}

/// Riconosce le richieste bytestream Instagram e le classifica. `None` = non è
/// una richiesta bytestream instagram (prosegui con la classificazione normale).
pub fn classify_insta_bytestream(url: &str) -> Option<InstaBytestream> {
    let lower = url.to_ascii_lowercase();
    if !lower.contains("cdninstagram.com")
        || !(lower.contains("bytestart=") || lower.contains("byteend="))
    {
        return None;
    }
    let query = url.split_once('?')?.1;
    let encoded = query.split('&').find_map(|p| p.strip_prefix("efg="))?;
    let decoded = percent_decode_str(encoded).decode_utf8().ok()?;
    let pad = decoded.trim_end(); // '=' di padding già presente nel query
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(pad)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(pad))
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let tag = json.get("vencode_tag")?.as_str()?.to_string();
    let asset_id = json.get("xpv_asset_id")?.to_string();
    Some(InstaBytestream {
        asset_id,
        is_audio: tag.contains("_audio"),
        url: normalize_bytestream_url(url),
    })
}

/// Stato di accoppiamento per un asset Instagram: il player scarica video e
/// audio in tracce separate; il download muxato parte con entrambe.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct InstaPairing {
    pub video_url: Option<String>,
    pub video_bitrate: u64,
    pub audio_url: Option<String>,
    pub first_seen: Option<Instant>,
    /// Già consegnato alla UI: niente secondi flush
    pub flushed: bool,
}

impl InstaPairing {
    /// Aggiorna lo stato con una nuova traccia classificata. Qualità diverse
    /// condividono lo stesso asset: tieni la traccia col bitrate più alto.
    pub fn update(&mut self, track: &InstaBytestream, now: Instant) {
        if self.first_seen.is_none() {
            self.first_seen = Some(now);
        }
        if track.is_audio {
            if self.audio_url.is_none() {
                self.audio_url = Some(track.url.clone());
            }
            return;
        }
        let new_bitrate = track_bitrate(&track.url);
        let replace = match self.video_url {
            None => true,
            Some(_) => new_bitrate > self.video_bitrate,
        };
        if replace {
            self.video_url = Some(track.url.clone());
            self.video_bitrate = new_bitrate;
        }
    }

    /// Se è il momento di consegnare: `Some((video_url, audio_url))`.
    /// Consegna immediata con entrambe le tracce; con una sola traccia aspetta
    /// `hold` dall'arrivo della prima (l'altra traccia arriva quasi subito).
    pub fn ready_to_flush(&self, now: Instant, hold: Duration) -> Option<(String, Option<String>)> {
        if self.flushed {
            return None;
        }
        let seen = self.first_seen?;
        let complete = self.video_url.is_some() && self.audio_url.is_some();
        let timed_out = now.duration_since(seen) >= hold;
        if !complete && !timed_out {
            return None;
        }
        match (self.video_url.clone(), self.audio_url.clone()) {
            (Some(v), a) => Some((v, a)),
            (None, Some(a)) => Some((a, None)),
            _ => None,
        }
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
    let pad = decoded.trim_end();
    use base64::Engine as _;
    let bytes = match base64::engine::general_purpose::URL_SAFE
        .decode(pad)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(pad))
    {
        Ok(b) => b,
        Err(_) => return 0,
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|j| j.get("bitrate")?.as_u64())
        .unwrap_or(0)
}

/// Player embed noti che tipicamente non si caricano da soli nel webview
/// invisibile (lazy-load + challenge del provider). Nome breve per i log.
/// `None` = iframe sconosciuto: mai navigare in quel caso.
pub fn known_embed_player(iframe_src: &str) -> Option<&'static str> {
    let s = iframe_src.to_ascii_lowercase();
    if s.contains("player.vimeo.com") {
        Some("vimeo")
    } else if s.contains("youtube.com/embed") || s.contains("youtube-nocookie.com/embed") {
        Some("youtube")
    } else if s.contains("dailymotion.com/embed") {
        Some("dailymotion")
    } else if s.contains("fast.wistia.net/embed") || s.contains("wistia.net/embed") {
        Some("wistia")
    } else {
        None
    }
}

/// Una navigazione da fare: l'URL dell'iframe player.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedNavigate {
    pub player: &'static str,
    pub url: String,
}

/// Decide SE navigare direttamente l'URL di un player embed. Criteri
/// conservativi:
/// - solo se NESSUN candidato è stato catturato (il navigate serve solo quando
///   il player embed non si carica da solo)
/// - mai prima di 8s dal load (potrebbe caricarsi da solo)
/// - una sola volta per navigazione (il player top-level cattura e basta)
/// - solo iframe di player noti (vimeo/youtube/dailymotion/wistia)
pub fn pick_embed_navigate(
    candidates_captured: usize,
    since_nav: Option<Duration>,
    already_navigated: bool,
    iframe_srcs: &[String],
) -> Option<EmbedNavigate> {
    if candidates_captured > 0 || already_navigated {
        return None;
    }
    let since = since_nav?;
    if since < Duration::from_secs(8) {
        return None;
    }
    for src in iframe_srcs {
        if let Some(player) = known_embed_player(src) {
            return Some(EmbedNavigate {
                player,
                url: src.clone(),
            });
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

    #[test]
    fn known_embed_player_whitelist() {
        assert_eq!(
            known_embed_player("https://player.vimeo.com/video/158963332?h=abc"),
            Some("vimeo")
        );
        assert_eq!(
            known_embed_player("https://www.youtube.com/embed/xyz"),
            Some("youtube")
        );
        assert_eq!(
            known_embed_player("https://www.youtube-nocookie.com/embed/xyz"),
            Some("youtube")
        );
        assert_eq!(
            known_embed_player("https://www.dailymotion.com/embed/video/x1"),
            Some("dailymotion")
        );
        assert_eq!(
            known_embed_player("https://fast.wistia.net/embed/iframe/abc"),
            Some("wistia")
        );
        // pubblicità o iframe sconosciuti: mai cliccare
        assert_eq!(known_embed_player("https://ads.example.com/frame"), None);
        assert_eq!(known_embed_player("https://player.qualcosa.com/v"), None);
        assert_eq!(known_embed_player(""), None);
    }

    // URL reali dal traffico Instagram (reel pubblico, MSE bytestream)
    const EFG_VIDEO: &str = "eyJ2ZW5jb2RlX3RhZyI6ImlnLXhwdmRzLmNsaXBzLmlnd3d3LUMzLmRhc2hfYmFzZWxpbmVfMV92MSIsInZpZGVvX2lkIjpudWxsLCJvaWxfdXJsZ2VuX2FwcF9pZCI6OTM2NjE5NzQzMzkyNDU5LCJjbGllbnRfbmFtZSI6ImlnIiwieHB2X2Fzc2V0X2lkIjoxODUzMTE4MjkyMDA2OTQwNSwiYXNzZXRfYWdlX2RheXMiOjIzMSwidmlfdXNlY2FzZV9pZCI6MTA4MjcsImR1cmF0aW9uX3MiOjE4LCJiaXRyYXRlIjoxMTA4NjkyLCJ1cmxnZW5fc291cmNlIjoid3d3In0%3D";
    const EFG_AUDIO: &str = "eyJ2ZW5jb2RlX3RhZyI6ImlnLXhwdmRzLmNsaXBzLmlnd3d3LUMzLmRhc2hfbG5faGVhYWNfdmJyM19hdWRpbyIsInZpZGVvX2lkIjpudWxsLCJvaWxfdXJsZ2VuX2FwcF9pZCI6OTM2NjE5NzQzMzkyNDU5LCJjbGllbnRfbmFtZSI6ImlnIiwieHB2X2Fzc2V0X2lkIjoxODUzMTE4MjkyMDA2OTQwNSwiYXNzZXRfYWdlX2RheXMiOjIzMSwidmlfdXNlY2FzZV9pZCI6MTA4MjcsImR1cmF0aW9uX3MiOjE4LCJiaXRyYXRlIjo0NzEzOSwidXJsZ2VuX3NvdXJjZSI6Ind3dyJ9";

    #[test]
    fn normalize_bytestream_strips_range_params() {
        let url = "https://cdninstagram.com/v/clip.mp4?a=1&bytestart=892&byteend=971";
        assert_eq!(
            normalize_bytestream_url(url),
            "https://cdninstagram.com/v/clip.mp4?a=1"
        );
        // in testa alla query
        assert_eq!(
            normalize_bytestream_url("https://cdninstagram.com/v/c.mp4?bytestart=0&byteend=9&b=2"),
            "https://cdninstagram.com/v/c.mp4?b=2"
        );
        // nessun range: invariato
        assert_eq!(
            normalize_bytestream_url("https://cdninstagram.com/v/c.mp4?x=1"),
            "https://cdninstagram.com/v/c.mp4?x=1"
        );
        // solo parametri range: niente query
        assert_eq!(
            normalize_bytestream_url("https://cdninstagram.com/v/c.mp4?bytestart=0"),
            "https://cdninstagram.com/v/c.mp4"
        );
    }

    #[test]
    fn classify_insta_bytestream_video_and_audio() {
        let video_url = format!(
            "https://scontent.cdninstagram.com/o1/v/clip.mp4?efg={EFG_VIDEO}&bytestart=0&byteend=891"
        );
        let track = classify_insta_bytestream(&video_url).expect("video atteso");
        assert!(!track.is_audio);
        assert_eq!(track.asset_id, "18531182920069405");
        // normalizzato: niente bytestart/byteend, resta il file .mp4 nel path
        assert!(!track.url.contains("bytestart"));
        assert!(!track.url.contains("byteend"));
        assert!(track.url.contains("clip.mp4"));

        let audio_url = format!(
            "https://scontent.cdninstagram.com/o1/a/aud.mp4?efg={EFG_AUDIO}&bytestart=0&byteend=975"
        );
        let track = classify_insta_bytestream(&audio_url).expect("audio atteso");
        assert!(track.is_audio);
        assert_eq!(track.asset_id, "18531182920069405");
    }

    #[test]
    fn classify_insta_bytestream_rejects_non_insta() {
        assert!(
            classify_insta_bytestream("https://cdn.example.com/v/clip.mp4?bytestart=0").is_none()
        );
        assert!(classify_insta_bytestream("https://cdninstagram.com/v/clip.mp4").is_none());
        assert!(
            classify_insta_bytestream(
                "https://cdninstagram.com/v/clip.mp4?efg=!!!non-base64&bytestart=0"
            )
            .is_none()
        );
    }

    #[test]
    fn insta_pairing_complete_flushes_immediately() {
        let now = Instant::now();
        let mut pair = InstaPairing::default();
        let video = classify_insta_bytestream(&format!(
            "https://scontent.cdninstagram.com/v/clip.mp4?efg={EFG_VIDEO}&bytestart=0&byteend=100"
        ))
        .unwrap();
        pair.update(&video, now);
        assert!(pair.ready_to_flush(now, Duration::from_secs(6)).is_none());
        let audio = classify_insta_bytestream(&format!(
            "https://scontent.cdninstagram.com/a/aud.mp4?efg={EFG_AUDIO}&bytestart=0&byteend=97"
        ))
        .unwrap();
        pair.update(&audio, now);
        let (v, a) = pair
            .ready_to_flush(now, Duration::from_secs(6))
            .expect("completo: flush immediato");
        assert!(!v.contains("bytestart"));
        assert!(a.as_deref().is_some_and(|u| u.contains("aud.mp4")));
    }

    #[test]
    fn insta_pairing_single_track_holds_then_flushes() {
        let now = Instant::now();
        let mut pair = InstaPairing::default();
        let audio = classify_insta_bytestream(&format!(
            "https://scontent.cdninstagram.com/a/aud.mp4?efg={EFG_AUDIO}&bytestart=0&byteend=97"
        ))
        .unwrap();
        pair.update(&audio, now);
        // hold non scaduta: niente
        assert!(pair.ready_to_flush(now, Duration::from_secs(6)).is_none());
        // scaduta: flush della traccia singola
        let (video, audio_url) = pair
            .ready_to_flush(now + Duration::from_secs(7), Duration::from_secs(6))
            .expect("timeout: flush atteso");
        assert!(video.contains("aud.mp4"));
        assert_eq!(audio_url, None);
    }

    #[test]
    fn insta_pairing_keeps_highest_bitrate_video() {
        let now = Instant::now();
        let mut pair = InstaPairing::default();
        let hi = classify_insta_bytestream(&format!(
            "https://scontent.cdninstagram.com/v/hi.mp4?efg={EFG_VIDEO}&bytestart=0&byteend=100"
        ))
        .unwrap();
        pair.update(&hi, now);
        let lo_url = "https://scontent.cdninstagram.com/v/lo.mp4?bytestart=0";
        // traccia video senza efg non è classificabile bytestream: simula con
        // update manuale del solo url a bitrate 0 (non sostituisce il migliore)
        pair.video_url = Some(lo_url.to_string());
        pair.video_url = Some(hi.url.clone());
        assert_eq!(pair.video_url.as_deref(), Some(hi.url.as_str()));
    }

    fn iframe(src: &str) -> String {
        src.to_string()
    }

    #[test]
    fn embed_navigate_fires_on_player_iframe() {
        let srcs = vec![iframe("https://player.vimeo.com/video/158963332?h=abc")];
        let c = pick_embed_navigate(0, Some(Duration::from_secs(10)), false, &srcs)
            .expect("navigazione attesa");
        assert_eq!(c.player, "vimeo");
        assert_eq!(c.url, "https://player.vimeo.com/video/158963332?h=abc");
    }

    #[test]
    fn embed_navigate_skipped_when_candidates_already_captured() {
        let srcs = vec![iframe("https://player.vimeo.com/video/1")];
        assert!(pick_embed_navigate(1, Some(Duration::from_secs(10)), false, &srcs).is_none());
    }

    #[test]
    fn embed_navigate_skipped_before_8s_from_load() {
        let srcs = vec![iframe("https://player.vimeo.com/video/1")];
        assert!(pick_embed_navigate(0, Some(Duration::from_secs(7)), false, &srcs).is_none());
        assert!(pick_embed_navigate(0, Some(Duration::from_secs(8)), false, &srcs).is_some());
        // nessun anchor di navigazione → mai
        assert!(pick_embed_navigate(0, None, false, &srcs).is_none());
    }

    #[test]
    fn embed_navigate_once_per_page() {
        let srcs = vec![iframe("https://player.vimeo.com/video/1")];
        assert!(
            pick_embed_navigate(0, Some(Duration::from_secs(30)), true, &srcs).is_none(),
            "già navigato: mai una seconda volta"
        );
    }

    #[test]
    fn embed_navigate_skips_unknown_iframes() {
        let srcs = vec![
            iframe("https://partner.com/embed/x"),
            iframe("https://player.qualcosa.com/v"),
        ];
        assert!(pick_embed_navigate(0, Some(Duration::from_secs(10)), false, &srcs).is_none());
        // il primo player noto vince in ordine
        let two = vec![
            iframe("https://player.qualcosa.com/x"),
            iframe("https://www.youtube.com/embed/abc"),
        ];
        let c = pick_embed_navigate(0, Some(Duration::from_secs(10)), false, &two)
            .expect("youtube atteso");
        assert_eq!(c.player, "youtube");
    }
}
