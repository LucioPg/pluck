//! Parsing PURO degli eventi CDP `Network.responseReceived` → candidati media.
//! Nessuna dipendenza da COM/Windows: testabile con fixture JSON.

use super::MediaCandidate;

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
    classify(url, mime, resource_type)
}

pub fn classify(url: &str, mime: &str, resource_type: &str) -> Option<MediaCandidate> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let url_lower = url.to_ascii_lowercase();
    let mime_lower = mime.to_ascii_lowercase();

    if url_lower.ends_with(".m3u8") || mime_lower.contains("mpegurl") {
        return Some(stream(url, "m3u8"));
    }
    if url_lower.ends_with(".mpd") || mime_lower.contains("dash+xml") {
        return Some(stream(url, "mpd"));
    }

    if video_extension(&url_lower) {
        return Some(MediaCandidate { url: url.into(), is_stream: false });
    }
    if resource_type.eq_ignore_ascii_case("Media") {
        return Some(MediaCandidate { url: url.into(), is_stream: false });
    }
    None
}

fn stream(url: &str, _kind: &str) -> MediaCandidate {
    MediaCandidate { url: url.into(), is_stream: true }
}

fn video_extension(url_lower: &str) -> bool {
    let path = url_lower.split(['?', '#']).next().unwrap_or(url_lower);
    [".mp4", ".webm", ".mkv", ".mov", ".avi", ".m4v", ".mpg", ".mpeg", ".ts"]
        .iter()
        .any(|ext| path.ends_with(ext))
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
        Self { seen: Default::default(), count: 0, cap }
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
            "https://cdn.com/v/clip.mp4", "video/mp4", "Media",
        ))
        .expect("candidato atteso");
        assert!(!c.is_stream);
        assert_eq!(c.url, "https://cdn.com/v/clip.mp4");
    }

    #[test]
    fn mp4_via_xhr_is_not_stream() {
        // i player JS scaricano mp4 progressivi via XHR/Fetch con range request
        let c = candidate_from_response_received(&params(
            "https://cdn.com/v/movie.mp4", "application/octet-stream", "XHR",
        ))
        .expect("candidato atteso");
        assert!(!c.is_stream);
    }

    #[test]
    fn m3u8_by_extension_and_by_mime() {
        let c = candidate_from_response_received(&params(
            "https://cdn.com/hls/index.m3u8", "application/vnd.apple.mpegurl", "XHR",
        ))
        .expect("candidato atteso");
        assert!(c.is_stream);

        // playlist servita senza estensione: il mimeType la smaschera
        let c = candidate_from_response_received(&params(
            "https://cdn.com/hls/playlist", "application/vnd.apple.mpegurl", "Fetch",
        ))
        .expect("candidato atteso");
        assert!(c.is_stream);
    }

    #[test]
    fn mpd_by_extension_and_by_mime() {
        let c = candidate_from_response_received(&params(
            "https://cdn.com/dash/manifest.mpd", "application/dash+xml", "Fetch",
        ))
        .expect("candidato atteso");
        assert!(c.is_stream);
    }

    #[test]
    fn ignores_non_media_resources() {
        assert!(candidate_from_response_received(&params(
            "https://cdn.com/app.js", "application/javascript", "Script",
        ))
        .is_none());
        assert!(candidate_from_response_received(&params(
            "https://cdn.com/img.png", "image/png", "Image",
        ))
        .is_none());
    }

    #[test]
    fn ignores_blob_and_data_urls() {
        assert!(candidate_from_response_received(&params(
            "blob:https://cdn.com/1234", "video/mp4", "Media",
        ))
        .is_none());
        assert!(candidate_from_response_received(&params(
            "data:video/mp4;base64,AAAA", "video/mp4", "Media",
        ))
        .is_none());
    }

    #[test]
    fn malformed_params_return_none() {
        assert!(candidate_from_response_received(&json!({})).is_none());
        assert!(candidate_from_response_received(&json!({ "response": {} })).is_none());
    }

    #[test]
    fn candidate_set_dedups_and_caps() {
        let mut set = CandidateSet::new(2);
        let a = MediaCandidate { url: "https://a.com/1".into(), is_stream: false };
        let b = MediaCandidate { url: "https://a.com/2".into(), is_stream: true };

        assert!(set.insert(&a));
        assert!(!set.insert(&a), "dedup per URL");
        assert!(set.insert(&b));
        assert_eq!(set.len(), 2);

        let c = MediaCandidate { url: "https://a.com/3".into(), is_stream: false };
        assert!(!set.insert(&c), "cap raggiunto: inoltro bloccato");
        assert_eq!(set.len(), 2);
    }
}
