//! Download bytestream Meta (Instagram/Facebook): video e audio arrivano come
//! tracce MSE separate, scaricate a pezzi con range request firmati. Scarica
//! OGNI range visto dal player in ordine di bytestart (concatenandoli: la
//! copertura dei byte = file completo) e, se c'e' la traccia audio separata,
//! la muxa con ffmpeg (-c copy, nessuna ricodifica).
//! Pausa non supportata (kill di un processo ffmpeg su Windows e' fragile).

use super::hls::ensure_ffmpeg;
use super::types::*;
use crate::browser::sniff::BytestreamSource;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub fn spawn_bytestream_mux_download(
    id: DownloadId,
    source: BytestreamSource,
    dest: PathBuf,
    tx: mpsc::Sender<ProgressMessage>,
) -> DownloadControl {
    let (pause_tx, _pause_rx) = watch::channel(false);
    let cancel = CancellationToken::new();
    let control = DownloadControl {
        cancel: cancel.clone(),
        pause: pause_tx,
    };
    tokio::spawn(run_bytestream_mux(id, source, dest, tx, cancel));
    control
}

async fn run_bytestream_mux(
    id: DownloadId,
    source: BytestreamSource,
    dest: PathBuf,
    tx: mpsc::Sender<ProgressMessage>,
    cancel: CancellationToken,
) {
    let report = |state: Option<DownloadState>, current: u64, total: u64| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(ProgressMessage {
                    id,
                    state,
                    current,
                    total,
                })
                .await;
        }
    };

    let v_tmp = part_path(&dest, "v");
    let a_tmp = part_path(&dest, "a");

    // 1) traccia video: concatena i range in ordine di bytestart
    let mut ranges = source.video_ranges.clone();
    ranges.sort_by_key(|r| r.start);
    let mut v_len = 0u64;
    let mut v_total = 0u64;
    for r in &ranges {
        if let Some(cl) = probe_length(&r.url).await {
            v_total += cl;
        }
    }
    report(None, 0, v_total).await;
    for r in &ranges {
        v_len += match download_range_to_file(&r.url, &v_tmp, cancel.clone(), &tx, id, v_len).await
        {
            Ok(l) => l,
            Err(e) => {
                fail(&tx, id, &e, &[&v_tmp, &a_tmp]).await;
                return;
            }
        };
        if cancel.is_cancelled() {
            cleanup(&[&v_tmp, &a_tmp]);
            return;
        }
    }

    // 2) traccia audio (se separata)
    let mut a_len = 0u64;
    if let Some(ar) = &source.audio_ranges {
        let mut sorted = ar.clone();
        sorted.sort_by_key(|r| r.start);
        for r in &sorted {
            if let Some(cl) = probe_length(&r.url).await {
                let _ = cl;
            }
        }
        for r in &sorted {
            a_len += match download_range_to_file(
                &r.url,
                &a_tmp,
                cancel.clone(),
                &tx,
                id,
                v_len + a_len,
            )
            .await
            {
                Ok(l) => l,
                Err(e) => {
                    fail(&tx, id, &e, &[&v_tmp, &a_tmp]).await;
                    return;
                }
            };
            if cancel.is_cancelled() {
                cleanup(&[&v_tmp, &a_tmp]);
                return;
            }
        }
    }
    report(None, v_len + a_len, v_len + a_len).await;

    // 3) mux ffmpeg (-c copy: nessuna ricodifica)
    let ff_path = match ensure_ffmpeg(&tx, id).await {
        Ok(p) => p,
        Err(e) => {
            cleanup(&[&v_tmp, &a_tmp]);
            let _ = report(Some(DownloadState::Failed(format!("ffmpeg: {e}"))), 0, 0).await;
            return;
        }
    };
    let mut cmd = ffmpeg_sidecar::command::FfmpegCommand::new_with_path(&ff_path);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y");
    cmd.arg("-i").arg(&v_tmp);
    if a_len > 0 {
        cmd.arg("-i").arg(&a_tmp);
    }
    cmd.arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&dest)
        .create_no_window();

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            cleanup(&[&v_tmp, &a_tmp]);
            let _ = report(
                Some(DownloadState::Failed(format!("ffmpeg launch: {e}"))),
                0,
                0,
            )
            .await;
            return;
        }
    };
    let child = Arc::new(Mutex::new(child));

    // Watcher di cancel: kill del processo -> wait esce
    {
        let child = child.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            if let Ok(mut guard) = child.try_lock() {
                let _ = guard.kill();
                let _ = guard.wait();
            }
        });
    }

    // Il merge -c copy e' veloce: aspetta l'exit e verifica
    let exit_status = {
        let mut guard = child.lock().unwrap();
        guard.wait()
    };
    let ok = matches!(exit_status, Ok(status) if status.success());
    cleanup(&[&v_tmp, &a_tmp]);
    if !ok || cancel.is_cancelled() {
        if cancel.is_cancelled() {
            let _ = std::fs::remove_file(&dest);
            return;
        }
        let _ = report(
            Some(DownloadState::Failed("mux ffmpeg fallito".into())),
            0,
            0,
        )
        .await;
        return;
    }
    let _ = report(Some(DownloadState::Completed), v_len + a_len, v_len + a_len).await;
}

/// Scarica un range (o l'URL intero) APPENDENDO al file: la concatenazione dei
/// range in ordine di bytestart ricostruisce il file completo.
async fn download_range_to_file(
    url: &str,
    to: &PathBuf,
    cancel: CancellationToken,
    tx: &mpsc::Sender<ProgressMessage>,
    id: DownloadId,
    base: u64,
) -> Result<u64, String> {
    let resp = reqwest::get(url)
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("GET: {e}"))?;
    let total = resp.content_length().unwrap_or(0);
    let mut stream = resp.bytes_stream();
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(to)
        .await
        .map_err(|e| format!("open: {e}"))?;
    let mut current: u64 = 0;
    let mut last_report = std::time::Instant::now();
    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            return Err("annullato".into());
        }
        let chunk = chunk.map_err(|e| format!("stream: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write: {e}"))?;
        current += chunk.len() as u64;
        if last_report.elapsed() >= std::time::Duration::from_millis(300) {
            let _ = tx
                .send(ProgressMessage {
                    id,
                    state: None,
                    current: base + current,
                    total: base + total,
                })
                .await;
            last_report = std::time::Instant::now();
        }
    }
    file.flush().await.map_err(|e| format!("flush: {e}"))?;
    let _ = tx
        .send(ProgressMessage {
            id,
            state: None,
            current: base + current,
            total: base + total,
        })
        .await;
    Ok(current)
}

/// Content-Length del file completo (la firma resta valida per il range).
async fn probe_length(url: &str) -> Option<u64> {
    let resp = reqwest::Client::new().head(url).send().await.ok()?;
    resp.headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
}

async fn fail(tx: &mpsc::Sender<ProgressMessage>, id: DownloadId, err: &str, paths: &[&PathBuf]) {
    cleanup(paths);
    let _ = tx
        .send(ProgressMessage {
            id,
            state: Some(DownloadState::Failed(err.to_string())),
            current: 0,
            total: 0,
        })
        .await;
}

fn part_path(dest: &Path, tag: &str) -> PathBuf {
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "pluck".into());
    let dir = dest.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    dir.join(format!(".{stem}.pluck-{tag}.part"))
}

fn cleanup(paths: &[&PathBuf]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bytestream_mux_downloads_tracks_and_merges() {
        // ffmpeg (installato sul sistema o sidecar): genera video e audio validi
        let ff_path = if ffmpeg_sidecar::command::ffmpeg_is_installed() {
            ffmpeg_sidecar::paths::ffmpeg_path()
                .to_string_lossy()
                .to_string()
        } else {
            std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into())
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let v_src = tmp.path().join("v_src.mp4");
        let a_src = tmp.path().join("a_src.m4a");
        ffmpeg_sidecar::command::FfmpegCommand::new_with_path(&ff_path)
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=128x96:rate=10",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&v_src)
            .spawn()
            .expect("spawn gen video")
            .wait()
            .expect("gen video");
        ffmpeg_sidecar::command::FfmpegCommand::new_with_path(&ff_path)
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1", "-y"])
            .arg(&a_src)
            .spawn()
            .expect("spawn gen audio")
            .wait()
            .expect("gen audio");
        let v_bytes = std::fs::read(&v_src).expect("leggi video");
        let v_len = v_bytes.len() as u64;
        let a_bytes = std::fs::read(&a_src).expect("leggi audio");
        let a_len = a_bytes.len() as u64;
        assert!(v_bytes.len() > 100 && a_bytes.len() > 100);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v.mp4"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(v_bytes))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/a.m4a"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(a_bytes))
            .mount(&server)
            .await;

        let (tx, mut rx) = mpsc::channel(50);
        let dest = tmp.path().join("out.mp4");
        let ctl = spawn_bytestream_mux_download(
            1,
            BytestreamSource {
                provider: crate::browser::sniff::BytestreamProvider::Instagram,
                video_ranges: vec![crate::browser::sniff::BytestreamRange {
                    start: 0,
                    end: v_len - 1,
                    url: format!("{}/v.mp4", server.uri()),
                }],
                audio_ranges: Some(vec![crate::browser::sniff::BytestreamRange {
                    start: 0,
                    end: a_len - 1,
                    url: format!("{}/a.m4a", server.uri()),
                }]),
            },
            dest.clone(),
            tx,
        );

        let mut completed = false;
        while let Some(msg) = rx.recv().await {
            match msg.state {
                Some(DownloadState::Completed) => {
                    completed = true;
                    break;
                }
                Some(DownloadState::Failed(m)) => panic!("mux fallito: {m}"),
                _ => {}
            }
        }
        assert!(completed, "atteso Completed");
        assert!(dest.exists(), "dest deve esistere");
        assert!(std::fs::metadata(&dest).expect("meta").len() > 0);
        // i file .part temporanei sono stati rimossi
        assert!(!tmp.path().join("out.pluck-v.part").exists());
        assert!(!tmp.path().join("out.pluck-a.part").exists());
        let _ = ctl;
    }
}
