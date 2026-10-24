//! Download HLS (m3u8) via ffmpeg sidecar: il processo scarica i segmenti e
//! li remuxa in MP4 con `-c copy` (zero ricodifica, qualità identica).
//!
//! Scelte (design approvato):
//! - ffmpeg fa TUTTO il flusso HLS (master/media playlist, variant selection,
//!   AES-128, EXT-X-MAP): lo stack HLS di ffmpeg è molto più solido di un
//!   parser fatto in casa. Progresso da `-progress pipe:1` (out_time vs
//!   durata totale letta dal manifest) → barra % reale
//! - ffmpeg binario scaricato UNA volta per macchina dal crate sidecar sotto
//!   la sua cache; uso successivo = avvio immediato
//! - Pausa non supportata (kill di un processo ffmpeg su Windows è fragile):
//!   solo Annulla (kill + pulizia file parziale)

use crate::download::types::*;
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use ffmpeg_sidecar::iter::FfmpegIterator;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// UA da browser: alcuni CDN rifiutano lo UA di default di ffmpeg (Lavf/…).
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Max righe di errore ffmpeg conservate per il messaggio `Failed`.
const ERR_TAIL: usize = 6;

/// Da chiamare dentro un runtime tokio (come `spawn_download`).
pub fn spawn_hls_download(
    id: DownloadId,
    url: String,
    referer: Option<String>,
    dest: PathBuf,
    tx: mpsc::Sender<ProgressMessage>,
) -> DownloadControl {
    // Pausa non supportata: canale fittizio (l'UI non mostra il bottone)
    let (pause_tx, _pause_rx) = watch::channel(false);
    let cancel = CancellationToken::new();
    let control = DownloadControl { cancel: cancel.clone(), pause: pause_tx };
    tokio::spawn(run_hls(id, url, referer, dest, tx, cancel));
    control
}

async fn run_hls(
    id: DownloadId,
    url: String,
    referer: Option<String>,
    dest: PathBuf,
    tx: mpsc::Sender<ProgressMessage>,
    cancel: CancellationToken,
) {
    let report = |state: Option<DownloadState>, current: u64, total: u64| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(ProgressMessage { id, state, current, total })
                .await;
        }
    };

    // Durata totale dal manifest (per la %); None → barra indeterminata
    let total = fetch_duration(&url, referer.as_deref()).await;
    report(None, 0, total.unwrap_or(0)).await;

    // ffmpeg: cache nostra → PATH/accanto all'exe → download una tantum con
    // progresso in byte (il default del sidecar scrive accanto all'exe: in
    // Program Files non è scrivibile)
    let ff_path = match ensure_ffmpeg(&tx, id).await {
        Ok(p) => p,
        Err(e) => {
            let _ = tx
                .send(ProgressMessage {
                    id,
                    state: Some(DownloadState::Failed(format!("ffmpeg: {e}"))),
                    current: 0,
                    total: 0,
                })
                .await;
            return;
        }
    };

    // Comando: remux diretto, nessuna ricodifica
    let mut cmd = FfmpegCommand::new_with_path(&ff_path);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-progress")
        .arg("pipe:1")
        .arg("-user_agent")
        .arg(USER_AGENT);
    if let Some(r) = &referer {
        cmd.arg("-headers").arg(format!("Referer: {r}\r\n"));
    }
    cmd.arg("-i")
        .arg(url)
        .arg("-c")
        .arg("copy")
        .arg("-y")
        .arg(&dest)
        .create_no_window();

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(ProgressMessage {
                    id,
                    state: Some(DownloadState::Failed(format!("Avvio ffmpeg: {e}"))),
                    current: 0,
                    total: 0,
                })
                .await;
            return;
        }
    };
    // L'iterator consuma stdout/stderr; il child resta per kill/wait
    let iterator = match FfmpegIterator::new(&mut child) {
        Ok(it) => it,
        Err(e) => {
            let _ = tx
                .send(ProgressMessage {
                    id,
                    state: Some(DownloadState::Failed(format!("ffmpeg: {e}"))),
                    current: 0,
                    total: 0,
                })
                .await;
            return;
        }
    };
    let child = Arc::new(Mutex::new(child));

    // Watcher di cancel: kill del processo → l'iterator riceve EOF → esce
    {
        let child = child.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            if let Ok(mut c) = child.lock() {
                let _ = c.kill();
            }
        });
    }

    // Loop eventi su thread blocking (readline sincrona)
    let cancel_loop = cancel.clone();
    let tx_loop = tx.clone();
    let id_loop = id;
    let outcome = tokio::task::spawn_blocking(move || {
        let mut cancelled = false;
        let mut err_tail: Vec<String> = Vec::new();
        let mut last_secs = 0u64;
        for ev in iterator {
            if cancel_loop.is_cancelled() {
                cancelled = true;
                if let Ok(mut c) = child.lock() {
                    let _ = c.kill();
                }
                break;
            }
            match ev {
                FfmpegEvent::Progress(p) => {
                    let secs = parse_progress_time(&p.time) as u64;
                    // Throttle 1s: riduce il churn di render sull'UI
                    if secs > last_secs {
                        last_secs = secs;
                        let _ = tx_loop.blocking_send(ProgressMessage {
                            id: id_loop,
                            state: None,
                            current: secs,
                            total: 0, // 0 = non sovrascrivere il totale noto
                        });
                    }
                }
                FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, m) => {
                    err_tail.push(m);
                    if err_tail.len() > ERR_TAIL {
                        err_tail.remove(0);
                    }
                }
                FfmpegEvent::Error(m) => err_tail.push(m),
                FfmpegEvent::Done => break,
                _ => {}
            }
        }
        let exit_ok = child
            .lock()
            .ok()
            .and_then(|mut c| c.wait().ok())
            .is_some_and(|s| s.success());
        (cancelled, exit_ok, err_tail)
    })
    .await
    .unwrap_or((false, false, vec!["thread ffmpeg terminato".into()]));

    if outcome.0 {
        // Annullato: pulizia del file parziale
        let _ = tokio::fs::remove_file(&dest).await;
        let _ = tx
            .send(ProgressMessage {
                id,
                state: Some(DownloadState::Cancelled),
                current: 0,
                total: 0,
            })
            .await;
    } else if outcome.1 && dest.exists() {
        let _ = tx
            .send(ProgressMessage {
                id,
                state: Some(DownloadState::Completed),
                current: total.unwrap_or(0),
                total: total.unwrap_or(0),
            })
            .await;
    } else {
        let _ = tokio::fs::remove_file(&dest).await;
        let mut reason = outcome.2.join(" | ");
        if reason.is_empty() {
            reason = "ffmpeg è terminato senza completare il file".into();
        }
        let _ = tx
            .send(ProgressMessage {
                id,
                state: Some(DownloadState::Failed(reason)),
                current: 0,
                total: 0,
            })
            .await;
    }
}

/// Cartella del binario ffmpeg (persistente, FUORI dal profilo cancellato
/// all'uscita: è uno strumento, non dati utente).
fn ffmpeg_dir() -> PathBuf {
    dirs::cache_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("ldwhelper")
        .join("ffmpeg")
}

/// Risolve il binario ffmpeg: cache nostra → già installato (PATH o accanto
/// all'exe) → scarica una tantum nella cache, con progresso sul canale.
async fn ensure_ffmpeg(
    tx: &mpsc::Sender<ProgressMessage>,
    id: DownloadId,
) -> Result<PathBuf, String> {
    let custom = ffmpeg_dir().join("ffmpeg.exe");
    if custom.exists() {
        return Ok(custom);
    }
    if ffmpeg_sidecar::command::ffmpeg_is_installed() {
        return Ok(ffmpeg_sidecar::paths::ffmpeg_path());
    }
    let ff_dir = ffmpeg_dir();
    tokio::fs::create_dir_all(&ff_dir)
        .await
        .map_err(|e| format!("cartella ffmpeg: {e}"))?;
    let tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        let url = ffmpeg_sidecar::download::ffmpeg_download_url()
            .map_err(|e| e.to_string())?;
        let archive =
            ffmpeg_sidecar::download::download_ffmpeg_package_with_progress(
                url,
                &ff_dir,
                |ev| {
                    // Progresso in byte: riusa la barra della riga download
                    if let ffmpeg_sidecar::download::FfmpegDownloadProgressEvent::Downloading {
                        total_bytes,
                        downloaded_bytes,
                    } = ev
                    {
                        let _ = tx.blocking_send(ProgressMessage {
                            id,
                            state: None,
                            current: downloaded_bytes,
                            total: total_bytes,
                        });
                    }
                },
            )
            .map_err(|e| e.to_string())?;
        ffmpeg_sidecar::download::unpack_ffmpeg_without_extras(&archive, &ff_dir)
            .map_err(|e| e.to_string())?;
        Ok(ff_dir.join("ffmpeg.exe"))
    })
    .await
    .map_err(|e| format!("task ffmpeg: {e}"))?
}

/// "00:03:29.04" → secondi (f64). Formato `-progress` di ffmpeg.
fn parse_progress_time(s: &str) -> f64 {
    let mut secs = 0f64;
    for part in s.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().unwrap_or(0.0);
    }
    secs
}

/// Durata totale del contenuto: media playlist → somma EXTINF; master →
/// variante a bandwidth maggiore → ricorsione. None = indeterminabile.
async fn fetch_duration(url: &str, referer: Option<&str>) -> Option<u64> {
    let text = fetch_text(url, referer).await?;
    match m3u8_rs::parse_playlist_res(text.as_bytes()) {
        Ok(m3u8_rs::Playlist::MasterPlaylist(master)) => {
            // Variante di qualità massima = bandwidth maggiore
            let best = master.variants.iter().max_by_key(|v| v.bandwidth)?;
            let uri = resolve_uri(url, &best.uri)?;
            let inner = fetch_text(&uri, referer).await?;
            sum_media(&inner).map(|d| d as u64)
        }
        Ok(m3u8_rs::Playlist::MediaPlaylist(_)) => sum_media(&text).map(|d| d as u64),
        Err(_) => None,
    }
}

/// Somma delle durate EXTINF dei segmenti (VOD). None se il parse fallisce.
fn sum_media(text: &str) -> Option<f64> {
    let pl = m3u8_rs::parse_media_playlist_res(text.as_bytes()).ok()?;
    Some(pl.segments.iter().map(|s| s.duration as f64).sum())
}

async fn fetch_text(url: &str, referer: Option<&str>) -> Option<String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let mut req = client.get(url);
    if let Some(r) = referer {
        req = req.header("Referer", r);
    }
    req.send().await.ok()?.text().await.ok()
}

/// URI relativo della variante/segmento → assoluto contro l'URL del manifest.
fn resolve_uri(base: &str, uri: &str) -> Option<String> {
    reqwest::Url::parse(base)
        .ok()?
        .join(uri)
        .ok()
        .map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_time_parses_hms() {
        assert_eq!(parse_progress_time("00:00:05.00"), 5.0);
        assert_eq!(parse_progress_time("00:03:29.04"), 209.04);
        assert_eq!(parse_progress_time("01:00:00.00"), 3600.0);
        assert_eq!(parse_progress_time("garbage"), 0.0);
    }

    #[test]
    fn media_playlist_duration_sum() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:9.5,\na.ts\n#EXTINF:10.5,\nb.ts\n#EXT-X-ENDLIST\n";
        assert_eq!(sum_media(text), Some(20.0));
        assert_eq!(sum_media("not a playlist"), None);
    }

    #[test]
    fn resolve_uri_joins_relative() {
        assert_eq!(
            resolve_uri("https://cdn.com/hls/index.m3u8", "seg01.ts").as_deref(),
            Some("https://cdn.com/hls/seg01.ts")
        );
        assert_eq!(
            resolve_uri("https://cdn.com/hls/index.m3u8", "https://altro.com/v.m3u8").as_deref(),
            Some("https://altro.com/v.m3u8")
        );
        assert_eq!(resolve_uri("not a url", "seg.ts"), None);
    }
}
