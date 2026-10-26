//! Modalità headless da linea di comando. Nessun argomento → UI grafica.
//!
//! Sintassi: `pluck <url> [referrer] [on|off]`. Spoofing attivo di default
//! (coerente con la UI): con spoofing `on` esplicito senza referrer → errore.

use crate::browser::{extension_from_url, format_bytes, name_from_url, sanitize_filename};
use crate::download::engine::spawn_download;
use crate::download::hls::spawn_hls_download;
use crate::download::types::*;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Argomenti CLI validati.
#[derive(Debug, PartialEq)]
pub struct CliArgs {
    pub url: String,
    pub referer: Option<String>,
    /// Invio dell'header Referer (spoofing). `false` → nessun Referer sui download.
    pub spoofing: bool,
}

/// Esito del parsing degli argomenti.
#[derive(Debug, PartialEq)]
pub enum CliAction {
    /// Nessun argomento: avvia la UI grafica.
    Ui,
    /// Stampa l'usage ed esce con 0.
    Help,
    /// Download diretto senza UI.
    Download(CliArgs),
}

pub fn usage() -> String {
    "\
pluck — download manager

Usage:
  pluck                             Launch the graphical UI
  pluck <url> [referrer] [spoofing] Direct download (no UI)
  pluck <url> <spoofing>            No referrer: on|off only

Arguments:
  url       URL of the file or m3u8/mpd manifest (.m3u8/.mpd → HLS via ffmpeg)
  referrer  Referrer to send with the requests (optional)
  spoofing  on|off — sends the Referer header (default: on; on requires a referrer)

Examples:
  pluck https://example.com/video.mp4
  pluck https://example.com/video.mp4 https://example.com/
  pluck https://example.com/video.mp4 https://example.com/ on
  pluck https://example.com/video.mp4 off
"
    .to_string()
}

/// Parsing: `Err(msg)` = errore mostrabile all'utente (exit code 2).
pub fn parse(args: &[String]) -> Result<CliAction, String> {
    if args.is_empty() {
        return Ok(CliAction::Ui);
    }
    if args.len() == 1 && (args[0] == "-h" || args[0] == "--help") {
        return Ok(CliAction::Help);
    }
    if args.len() > 3 {
        return Err(format!(
            "Too many arguments (max 3: url, referrer, spoofing): {}",
            args.len()
        ));
    }
    let url = args[0].clone();
    reqwest::Url::parse(&url).map_err(|_| format!("Invalid URL: {url}"))?;

    /// `Some(Some(on))` = on/off riconosciuto; `Some(None)` = non è on/off.
    fn parse_spoof(v: &str) -> Option<Option<bool>> {
        match v.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => Some(Some(true)),
            "off" | "false" | "0" => Some(Some(false)),
            _ => None,
        }
    }

    // 2° argomento: on|off diretto (senza referrer) oppure referrer
    let (referer, spoofing) = if let Some(v) = args.get(1)
        && let Some(sp) = parse_spoof(v)
    {
        if args.len() > 2 {
            return Err("Too many arguments: with direct spoofing, max 2 (url, spoofing)".into());
        }
        (None, sp)
    } else {
        let referer = match args.get(1) {
            Some(r) => {
                reqwest::Url::parse(r).map_err(|_| format!("Invalid referrer: {r}"))?;
                Some(r.clone())
            }
            None => None,
        };
        let spoofing = match args.get(2) {
            Some(v) => parse_spoof(v)
                .ok_or_else(|| format!("Unrecognized spoofing value: {v} (use on|off)"))?,
            None => None,
        };
        (referer, spoofing)
    };
    // Default: on (coerente con la UI). `on` esplicito senza referrer = contraddizione.
    if spoofing == Some(true) && referer.is_none() {
        return Err("spoofing on requires a referrer".into());
    }
    Ok(CliAction::Download(CliArgs {
        url,
        referer,
        spoofing: spoofing.unwrap_or(true),
    }))
}

/// Rileva playlist HLS/DASH dall'estensione del path (`extension_from_url` non
/// va bene: m3u8/mpd sono proprio i casi che scarta).
pub fn is_hls_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path().rsplit('/').next().map(|s| {
                let low = s.to_ascii_lowercase();
                low.ends_with(".m3u8") || low.ends_with(".mpd")
            })
        })
        .unwrap_or(false)
}

/// Percorso di destinazione: `<Downloads>/<nome pulito><est>`, senza sovrascrivere
/// file esistenti (suffisso ` (n)`). Fallback `.` se la cartella Download non è nota.
pub fn dest_path(url: &str) -> PathBuf {
    let base = sanitize_filename(&name_from_url(url, true));
    // Il nome già include l'estensione → non duplicarla ("video.mp4", mai
    // "video.mp4.mp4"); m3u8/mpd restano senza estensione (ffmpeg remuxa in MP4)
    let ext = extension_from_url(url);
    let name = if base
        .to_ascii_lowercase()
        .ends_with(&ext.to_ascii_lowercase())
    {
        base
    } else {
        format!("{base}{ext}")
    };
    let dir = dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));
    unique_path(dir.join(name))
}

/// Se `path` esiste già ritorna il primo libero: `nome (1).ext`, `nome (2).ext`…
fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    for n in 1..10_000u32 {
        let cand = if ext.is_empty() {
            dir.join(format!("{stem} ({n})"))
        } else {
            dir.join(format!("{stem} ({n}).{ext}"))
        };
        if !cand.exists() {
            return cand;
        }
    }
    path
}

/// Download headless: progresso su stdout, esito finale, exit code (0 = ok).
pub async fn download(args: CliArgs) -> i32 {
    // Spoofing disattivato → nessun Referer, qualunque referrer sia stato dato.
    let referer = if args.spoofing {
        args.referer.clone()
    } else {
        None
    };
    let is_hls = is_hls_url(&args.url);
    let dest = dest_path(&args.url);

    println!("Download: {}", args.url);
    println!("Destinazione: {}", dest.display());
    match &referer {
        Some(r) => println!("Referer: {r}"),
        None => println!("Referer: (none)"),
    }
    if is_hls {
        println!("HLS/DASH stream: converting to MP4 via ffmpeg");
    }

    let (tx, mut rx) = mpsc::channel::<ProgressMessage>(100);
    let _ctl = if is_hls {
        spawn_hls_download(1, args.url.clone(), referer, dest.clone(), tx)
    } else {
        spawn_download(1, args.url.clone(), referer, dest.clone(), tx)
    };

    let mut last_pct: Option<u64> = None;
    while let Some(msg) = rx.recv().await {
        if let Some(state) = msg.state {
            return match state {
                DownloadState::Completed => {
                    println!("\r\x1b[KCompleted: {}", dest.display());
                    0
                }
                DownloadState::Failed(m) => {
                    eprintln!("\r\x1b[KFailed: {m}");
                    1
                }
                DownloadState::Cancelled => {
                    eprintln!("\r\x1b[KCancelled");
                    1
                }
                DownloadState::Downloading | DownloadState::Paused => continue,
            };
        }
        // Progresso: percentuale per file diretti (current/total in byte),
        // tempo per HLS (current/total in secondi)
        if msg.total > 0 {
            let line = if is_hls {
                format!(
                    "\r\x1b[K{} / {}",
                    format_time(msg.current),
                    format_time(msg.total)
                )
            } else {
                let pct = (msg.current * 100).checked_div(msg.total).unwrap_or(0);
                format!(
                    "\r\x1b[K{}% ({} / {})",
                    pct,
                    format_bytes(msg.current),
                    format_bytes(msg.total)
                )
            };
            let pct = msg
                .current
                .checked_mul(100)
                .and_then(|p| p.checked_div(msg.total));
            if pct != last_pct || is_hls {
                last_pct = pct;
                print!("{line}");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
            }
        }
    }
    eprintln!("\r\x1b[KDownload ended without a result (channel closed)");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Result<CliAction, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&owned)
    }

    #[test]
    fn no_args_starts_ui() {
        assert_eq!(parse_str(&[]).unwrap(), CliAction::Ui);
    }

    #[test]
    fn help_flags() {
        assert_eq!(parse_str(&["-h"]).unwrap(), CliAction::Help);
        assert_eq!(parse_str(&["--help"]).unwrap(), CliAction::Help);
    }

    #[test]
    fn url_only_spoofing_defaults_on_without_referer() {
        // Default on ma nessun referrer: nessun errore, nessun header inviato
        let CliAction::Download(a) = parse_str(&["https://a.com/v.mp4"]).unwrap() else {
            panic!("atteso Download");
        };
        assert_eq!(a.url, "https://a.com/v.mp4");
        assert_eq!(a.referer, None);
        assert!(a.spoofing);
    }

    #[test]
    fn url_and_referer() {
        let CliAction::Download(a) = parse_str(&["https://a.com/v.mp4", "https://a.com/"]).unwrap()
        else {
            panic!("atteso Download");
        };
        assert_eq!(a.referer.as_deref(), Some("https://a.com/"));
        assert!(a.spoofing);
    }

    #[test]
    fn spoofing_direct_off_without_referer() {
        let CliAction::Download(a) = parse_str(&["https://a.com/v.mp4", "off"]).unwrap() else {
            panic!("atteso Download");
        };
        assert_eq!(a.referer, None);
        assert!(!a.spoofing);
    }

    #[test]
    fn spoofing_off_with_referer() {
        let CliAction::Download(a) =
            parse_str(&["https://a.com/v.mp4", "https://a.com/", "off"]).unwrap()
        else {
            panic!("atteso Download");
        };
        assert_eq!(a.referer.as_deref(), Some("https://a.com/"));
        assert!(!a.spoofing);
    }

    #[test]
    fn explicit_on_without_referer_is_error() {
        let err = parse_str(&["https://a.com/v.mp4", "on"]).unwrap_err();
        assert!(err.contains("requires a referrer"), "errore: {err}");
    }

    #[test]
    fn spoofing_values_case_insensitive_and_synonyms() {
        for (v, expected) in [
            ("on", true),
            ("ON", true),
            ("true", true),
            ("1", true),
            ("off", false),
            ("OFF", false),
            ("false", false),
            ("0", false),
        ] {
            let CliAction::Download(a) =
                parse_str(&["https://a.com/v.mp4", "https://a.com/", v]).unwrap()
            else {
                panic!("atteso Download per {v}");
            };
            assert_eq!(a.spoofing, expected, "spoofing per {v}");
        }
    }

    #[test]
    fn spoofing_bad_value_is_error() {
        assert!(parse_str(&["https://a.com/v.mp4", "https://a.com/", "forse"]).is_err());
    }

    #[test]
    fn invalid_url_is_error() {
        assert!(parse_str(&["non e' un url"]).is_err());
    }

    #[test]
    fn invalid_referer_is_error() {
        assert!(parse_str(&["https://a.com/v.mp4", "non un url"]).is_err());
    }

    #[test]
    fn spoofing_direct_with_extra_arg_is_error() {
        assert!(parse_str(&["https://a.com/v.mp4", "off", "extra"]).is_err());
    }

    #[test]
    fn too_many_args_is_error() {
        assert!(parse_str(&["https://a.com/v.mp4", "https://a.com/", "on", "extra"]).is_err());
    }

    #[test]
    fn hls_detection_by_extension() {
        assert!(is_hls_url("https://a.com/index.m3u8"));
        assert!(is_hls_url("https://a.com/index.M3U8?token=1"));
        assert!(is_hls_url("https://a.com/manifest.mpd"));
        assert!(!is_hls_url("https://a.com/video.mp4"));
        assert!(!is_hls_url("https://a.com/playlist.txt"));
        assert!(!is_hls_url("non un url"));
    }

    #[test]
    fn unique_path_avoids_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("video.mp4");
        std::fs::write(&target, "x").unwrap();
        let first = unique_path(target.clone());
        assert_eq!(first, dir.path().join("video (1).mp4"));
        std::fs::write(&first, "x").unwrap();
        assert_eq!(unique_path(target), dir.path().join("video (2).mp4"));
        // Libero: invariato
        assert_eq!(
            unique_path(dir.path().join("altro.mp4")),
            dir.path().join("altro.mp4")
        );
    }

    #[test]
    fn dest_path_uses_clean_name_and_extension() {
        // Nome già con estensione: nessun raddoppio
        let p = dest_path("https://cdn.com/media/il%20mio%20video.mp4");
        assert_eq!(p.file_name().unwrap(), "il mio video.mp4");
        // Senza estensione: default .mp4
        let p = dest_path("https://cdn.com/media/clip");
        assert_eq!(p.file_name().unwrap(), "clip.mp4");
    }
}
