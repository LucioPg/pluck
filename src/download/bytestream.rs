//! Download bytestream Instagram: video e audio arrivano come tracce MSE
//! separate (file fMP4). Scarica entrambi e li muxa in MP4 con ffmpeg
//! (-c copy, nessuna ricodifica). Pausa non supportata (come HLS).

use super::hls::ensure_ffmpeg;
use super::types::*;
use futures_util::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub fn spawn_bytestream_mux_download(
    id: DownloadId,
    video_url: String,
    audio_url: Option<String>,
    dest: PathBuf,
    tx: mpsc::Sender<ProgressMessage>,
) -> DownloadControl {
    let (pause_tx, pause_rx) = watch::channel(false);
    let cancel = CancellationToken::new();
    let control = DownloadControl {
        cancel: cancel.clone(),
        pause: pause_tx,
    };
    tokio::spawn(run_bytestream_mux(
        id, video_url, audio_url, dest, tx, cancel, pause_rx,
    ));
    control
}

async fn run_bytestream_mux(
    id: DownloadId,
    video_url: String,
    audio_url: Option<String>,
    dest: PathBuf,
    tx: mpsc::Sender<ProgressMessage>,
    cancel: CancellationToken,
    _pause_rx: watch::Receiver<bool>,
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

    // File temporanei accanto alla destinazione (stesso volume: il rename
    // finale di ffmpeg output non serve, ffmpeg scrive direttamente su dest;
    // i temp servono solo per le tracce scaricate)
    let v_tmp = part_path(&dest, "v");
    let a_tmp = part_path(&dest, "a");

    // 1) traccia video
    report(None, 0, 0).await;
    let v_len = match download_to_file(&video_url, &v_tmp, &cancel, &tx, id, 0).await {
        Ok(l) => l,
        Err(e) => {
            fail_cancelled(&tx, id, &e, &v_tmp, &a_tmp).await;
            return;
        }
    };
    if cancel.is_cancelled() {
        cleanup(&[&v_tmp, &a_tmp]);
        return;
    }

    // 2) traccia audio (se separata)
    let mut a_len = 0u64;
    if let Some(audio_url) = &audio_url {
        a_len = match download_to_file(audio_url, &a_tmp, &cancel, &tx, id, v_len).await {
            Ok(l) => l,
            Err(e) => {
                fail_cancelled(&tx, id, &e, &v_tmp, &a_tmp).await;
                return;
            }
        };
        if cancel.is_cancelled() {
            cleanup(&[&v_tmp, &a_tmp]);
            return;
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
        .arg("-y")
        .arg("-i")
        .arg(&v_tmp);
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

    // Watcher di cancel: kill del processo → wait esce
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

    // Il merge -c copy è veloce: drena gli errori e aspetta l'exit
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

async fn download_to_file(
    url: &str,
    to: &PathBuf,
    cancel: &CancellationToken,
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
    let mut file = tokio::fs::File::create(to)
        .await
        .map_err(|e| format!("create: {e}"))?;
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

async fn fail_cancelled(
    tx: &mpsc::Sender<ProgressMessage>,
    id: DownloadId,
    err: &str,
    v_tmp: &PathBuf,
    a_tmp: &PathBuf,
) {
    cleanup(&[v_tmp, a_tmp]);
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
    use crate::download::types::DownloadState;
    use ffmpeg_sidecar::command::FfmpegCommand;

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
        FfmpegCommand::new_with_path(&ff_path)
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
        FfmpegCommand::new_with_path(&ff_path)
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1", "-y"])
            .arg(&a_src)
            .spawn()
            .expect("spawn gen audio")
            .wait()
            .expect("gen audio");
        let v_bytes = std::fs::read(&v_src).expect("leggi video");
        let a_bytes = std::fs::read(&a_src).expect("leggi audio");
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
            format!("{}/v.mp4", server.uri()),
            Some(format!("{}/a.m4a", server.uri())),
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
