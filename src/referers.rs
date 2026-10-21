//! Persistenza referer preferiti. Layer app: nessuna dipendenza UI.
//! File JSON `%APPDATA%/ldwhelper/referers.json`; assente/corrotto → lista vuota (mai panic).

use std::path::PathBuf;

pub struct RefererStore {
    path: PathBuf,
    entries: Vec<String>,
}

impl RefererStore {
    /// Percorso default: `<data_dir>/ldwhelper/referers.json` (Windows: %APPDATA%)
    /// Fallback `.` (CWD) se `dirs::data_dir()` è None (irraggiungibile su Windows).
    pub fn default_path() -> PathBuf {
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("ldwhelper").join("referers.json")
    }

    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn list(&self) -> &[String] {
        &self.entries
    }

    /// Dedup esatto + move-to-front. NON salva su disco: chiamare `save()`.
    pub fn record(&mut self, referer: &str) {
        let r = referer.trim().to_string();
        if r.is_empty() {
            return;
        }
        self.entries.retain(|e| e != &r);
        self.entries.insert(0, r);
    }

    /// Rimozione esatta. `false` se assente. NON salva su disco: chiamare `save()`.
    pub fn remove(&mut self, referer: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e != referer);
        self.entries.len() != before
    }

    /// L'UI può ignorare il Result: il file si riscriverà al prossimo save (best-effort).
    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create_dir_all: {e}"))?;
        }
        let json =
            serde_json::to_string_pretty(&self.entries).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(&self.path, json).map_err(|e| format!("write: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_gives_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = RefererStore::load(dir.path().join("referers.json"));
        assert!(store.list().is_empty());
    }

    #[test]
    fn load_invalid_json_gives_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("referers.json");
        std::fs::write(&p, "non e' json").unwrap();
        let store = RefererStore::load(p);
        assert!(store.list().is_empty());
    }

    #[test]
    fn record_dedup_moves_to_front() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RefererStore::load(dir.path().join("referers.json"));
        store.record("https://a.com");
        store.record("https://b.com");
        store.record("https://a.com");
        assert_eq!(
            store.list(),
            &["https://a.com".to_string(), "https://b.com".to_string()]
        );
    }

    #[test]
    fn record_empty_and_whitespace_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RefererStore::load(dir.path().join("referers.json"));
        store.record("   ");
        assert!(store.list().is_empty());
    }

    #[test]
    fn save_then_load_roundtrip_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sottocartella").join("referers.json");
        let mut store = RefererStore::load(p.clone());
        store.record("https://esempio.com");
        store.save().unwrap();
        let reloaded = RefererStore::load(p);
        assert_eq!(reloaded.list(), &["https://esempio.com".to_string()]);
    }

    #[test]
    fn remove_existing_returns_true_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RefererStore::load(dir.path().join("referers.json"));
        store.record("https://a.com");
        store.record("https://b.com");
        assert!(store.remove("https://a.com"));
        assert_eq!(store.list(), &["https://b.com".to_string()]);
    }

    #[test]
    fn remove_missing_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RefererStore::load(dir.path().join("referers.json"));
        store.record("https://a.com");
        assert!(!store.remove("https://assente.com"));
        assert_eq!(store.list().len(), 1);
    }
}
