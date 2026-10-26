//! Persistenza referer preferiti. Layer app: nessuna dipendenza UI.
//! File `%APPDATA%/pluck/referers.json`.
//!
//! Privacy: il file è cifrato con DPAPI Windows (chiave legata all'account
//! utente). In load viene accettato anche il plaintext legacy (migrazione
//! automatica: il primo save riscrive cifrato).
//!
//! Severità in lettura: file presente ma illeggibile/non decifrabile/JSON
//! non valido → lo store resta usabile in memoria MA la persistenza è
//! disabilitata (`corrupted()` = Some, `save()` = Err): l'utente vede il
//! path nel banner e il file corrotto non viene mai sovrascritto.

use std::path::{Path, PathBuf};

pub struct RefererStore {
    path: PathBuf,
    entries: Vec<String>,
    /// Some(motivo) = file presente ma non leggibile/decifrabile: save() bloccato.
    corrupted: Option<String>,
}

impl RefererStore {
    /// Percorso default: `<data_dir>/pluck/referers.json` (Windows: %APPDATA%)
    /// Fallback `.` (CWD) se `dirs::data_dir()` è None (irraggiungibile su Windows).
    pub fn default_path() -> PathBuf {
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("pluck").join("referers.json")
    }

    pub fn load(path: PathBuf) -> Self {
        match std::fs::read(&path) {
            // File assente = prima esecuzione: store nuovo, NON corrotto
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self {
                path,
                entries: Vec::new(),
                corrupted: None,
            },
            Err(e) => Self {
                path,
                entries: Vec::new(),
                corrupted: Some(format!("read failed: {e}")),
            },
            Ok(bytes) => {
                // Plaintext legacy prima (compat), poi blob DPAPI
                let parsed = serde_json::from_slice::<Vec<String>>(&bytes)
                    .map_err(|e| format!("plaintext: {e}"))
                    .or_else(|_| {
                        dpapi::unprotect(&bytes)
                            .and_then(|plain| {
                                serde_json::from_slice::<Vec<String>>(&plain)
                                    .map_err(|e| format!("decrypted blob is not valid JSON: {e}"))
                            })
                            .map_err(|e| format!("DPAPI decryption: {e}"))
                    });
                match parsed {
                    Ok(entries) => Self {
                        path,
                        entries,
                        corrupted: None,
                    },
                    Err(reason) => Self {
                        path,
                        entries: Vec::new(),
                        corrupted: Some(reason),
                    },
                }
            }
        }
    }

    pub fn list(&self) -> &[String] {
        &self.entries
    }

    /// Motivo della corruzione (persistenza disabilitata) se il file era illeggibile.
    pub fn corrupted(&self) -> Option<&str> {
        self.corrupted.as_deref()
    }

    /// Percorso del file (per il messaggio all'utente).
    pub fn path(&self) -> &Path {
        &self.path
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
        self.len() != before
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Cifra con DPAPI e scrive. Con file corrotto in load → Err (niente
    /// sovrascrittura: il file resta come prova per l'utente).
    /// L'UI può ignorare il Result: il file si riscriverà al prossimo save (best-effort).
    pub fn save(&self) -> Result<(), String> {
        if let Some(reason) = &self.corrupted {
            return Err(format!(
                "corrupted referer file, saving disabled ({reason})"
            ));
        }
        let json = serde_json::to_vec(&self.entries).map_err(|e| format!("serialize: {e}"))?;
        let blob = dpapi::protect(&json)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create_dir_all: {e}"))?;
        }
        std::fs::write(&self.path, blob).map_err(|e| format!("write: {e}"))
    }
}

/// DPAPI Windows: chiave legata all'account utente, nessun segreto nel binario.
/// Fallback plaintext-plaintext su non-Windows solo per compilabilità
/// (il target del progetto è windows-only).
#[cfg(windows)]
mod dpapi {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::w;

    /// Cifra con DPAPI (scope utente corrente, nessun prompt UI).
    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                w!("pluck referers"),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| format!("DPAPI protect: {e}"))?;
        }
        Ok(owned(out))
    }

    /// Decifra un blob DPAPI.
    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(data);
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| format!("DPAPI unprotect: {e}"))?;
        }
        Ok(owned(out))
    }

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copia il buffer nativo e lo rilascia (LocalFree).
    fn owned(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let slice = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) };
        let v = slice.to_vec();
        unsafe { LocalFree(Some(HLOCAL(blob.pbData as *mut _))) };
        v
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        Ok(data.to_vec())
    }
    pub fn unprotect(_data: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI disponibile solo su Windows".into())
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
        assert!(store.corrupted().is_none());
    }

    #[test]
    fn load_invalid_json_reports_corruption_and_blocks_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("referers.json");
        std::fs::write(&p, "non e' json").unwrap();
        let store = RefererStore::load(p.clone());
        assert!(store.list().is_empty());
        assert!(store.corrupted().is_some());
        // Il path del file è quello richiesto dall'utente per il banner
        assert_eq!(store.path(), p);
        // Salvataggio bloccato e file NON sovrascritto
        let before = std::fs::read(&p).unwrap();
        assert!(store.save().is_err());
        assert_eq!(std::fs::read(&p).unwrap(), before);
    }

    #[test]
    fn load_plaintext_legacy_then_resaves_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("referers.json");
        std::fs::write(&p, r#"["https://legacy.com"]"#).unwrap();
        let store = RefererStore::load(p.clone());
        assert!(store.corrupted().is_none());
        assert_eq!(store.list(), &["https://legacy.com".to_string()]);
        // Migrazione: il save riscrive CIFRATO (non più JSON plaintext)
        store.save().unwrap();
        let raw = std::fs::read(&p).unwrap();
        assert_ne!(raw, b"[\"https://legacy.com\"]".to_vec());
        // Ricarica dal cifrato: stesso contenuto
        let reloaded = RefererStore::load(p);
        assert!(reloaded.corrupted().is_none());
        assert_eq!(reloaded.list(), &["https://legacy.com".to_string()]);
    }

    #[test]
    fn load_random_bytes_reports_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("referers.json");
        // Né JSON né blob DPAPI valido
        std::fs::write(&p, [0u8, 159, 146, 150, 7, 42, 255, 13]).unwrap();
        let store = RefererStore::load(p);
        assert!(store.list().is_empty());
        assert!(store.corrupted().is_some());
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

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip_changes_bytes() {
        let data = b"payload di prova 123".to_vec();
        let enc = dpapi::protect(&data).unwrap();
        assert_ne!(enc, data);
        assert_eq!(dpapi::unprotect(&enc).unwrap(), data);
    }
}
