//! Motore download. SOLO reqwest/tokio/tokio-util — zero import UI (vincolo addon futuro).

use crate::download::types::*;
use futures_util::StreamExt;
use std::path::PathBuf;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Invia progress solo ogni THROTTLE_BYTES scaricati (riduce rumore sul canale).
const THROTTLE_BYTES: u64 = 64 * 1024;

pub fn spawn_download(
    id: DownloadId,
    url: String,
    referer: Option<String>,
    dest: PathBuf,
    tx: mpsc::Sender<ProgressMessage>,
) -> DownloadControl {
    let (pause_tx, pause_rx) = watch::channel(false);
    let cancel = CancellationToken::new();
    let control = DownloadControl { cancel: cancel.clone(), pause: pause_tx };
    tokio::spawn(run_download(id, url, referer, dest, tx, cancel, pause_rx));
    control
}

async fn run_download(
    id: DownloadId,
    url: String,
    referer: Option<String>,
    dest: PathBuf,
    tx: mpsc::Sender<ProgressMessage>,
    cancel: CancellationToken,
    mut pause_rx: watch::Receiver<bool>,  // mut: wait_for/changed richiedono &mut self
) {
    match attempt(&id, &url, &referer, &dest, 0, &tx, &cancel, &mut pause_rx).await {
        Attempt::Completed => {}
        Attempt::Cancelled => {}
        // Stream morto con byte scaricati → recovery Range una tantum
        Attempt::Disconnected { current } if current > 0 => {
            match attempt(&id, &url, &referer, &dest, current, &tx, &cancel, &mut pause_rx).await {
                Attempt::Completed | Attempt::Cancelled => {}
                Attempt::Disconnected { .. } => {
                    let _ = tx.send(ProgressMessage {
                        id,
                        state: Some(DownloadState::Failed(
                            "Connessione persa, recovery fallita".into(),
                        )),
                        current,
                        total: 0,
                    }).await;
                }
                Attempt::FailedAlreadyReported => {}
            }
        }
        Attempt::Disconnected { .. } => {
            let _ = tx.send(ProgressMessage {
                id,
                state: Some(DownloadState::Failed("Connessione persa".into())),
                current: 0,
                total: 0,
            }).await;
        }
        Attempt::FailedAlreadyReported => {}
    }
}

enum Attempt {
    Completed,
    Cancelled,
    /// Stream/connessione morto; `current` = byte salvati finora
    Disconnected { current: u64 },
    /// Errore terminale (HTTP, disco): motivo già inviato via canale
    FailedAlreadyReported,
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    id: &DownloadId,
    url: &str,
    referer: &Option<String>,
    dest: &PathBuf,
    offset: u64,
    tx: &mpsc::Sender<ProgressMessage>,
    cancel: &CancellationToken,
    pause_rx: &mut watch::Receiver<bool>,  // wait_for/changed: &mut self
) -> Attempt {
    let client = reqwest::Client::new();
    let mut request = client.get(url);
    if let Some(r) = referer {
        request = request.header("Referer", r);
    }
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(_) => return Attempt::Disconnected { current: offset },
    };
    let status = response.status();
    if !status.is_success() {
        let _ = tx.send(ProgressMessage {
            id: *id,
            state: Some(DownloadState::Failed(format!("HTTP {status}"))),
            current: offset,
            total: 0,
        }).await;
        return Attempt::FailedAlreadyReported;
    }

    // Recovery: server senza Range risponde 200 → riparte da zero (truncate)
    let mut current = if offset > 0 && status == reqwest::StatusCode::OK {
        0
    } else {
        offset
    };

    // Content-Length: su 206 = parte restante
    let content_len = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let total = if status == reqwest::StatusCode::PARTIAL_CONTENT {
        current + content_len
    } else {
        content_len
    };
    let _ = tx.send(ProgressMessage { id: *id, state: None, current, total }).await;

    let mut file = match OpenOptions::new()
        .create(true)
        .write(true)
        .append(current > 0) // recovery 206: append da EOF (== current: mai scritto oltre current)
        .truncate(current == 0)
        .open(dest)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.send(ProgressMessage {
                id: *id,
                state: Some(DownloadState::Failed(format!("Errore apertura file: {e}"))),
                current,
                total,
            }).await;
            return Attempt::FailedAlreadyReported;
        }
    };

    let mut stream = response.bytes_stream();
    let mut last_sent = current.saturating_sub(THROTTLE_BYTES);

    loop {
        // Check pausa a INIZIO loop: copre anche pausa richiesta PRIMA di entrare nel loop
        // (il guard di select! verrebbe valutato una sola volta e la perderebbe).
        if !cancel.is_cancelled() && *pause_rx.borrow_and_update() {
            let _ = tx.send(ProgressMessage {
                id: *id,
                state: Some(DownloadState::Paused),
                current,
                total,
            }).await;
            tokio::select! {
                // attesa resume NON blocca il cancel
                _ = pause_rx.wait_for(|p| !*p) => {}
                _ = cancel.cancelled() => { /* il braccio cancel del select esterno fa pulizia */ }
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = file.flush().await;
                drop(file);
                let _ = tokio::fs::remove_file(dest).await;
                let _ = tx.send(ProgressMessage {
                    id: *id,
                    state: Some(DownloadState::Cancelled),
                    current: 0,
                    total: 0,
                }).await;
                return Attempt::Cancelled;
            }
            // Sveglio su variazioni pausa: il check reale è a inizio loop
            _ = pause_rx.changed() => { continue; }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        if let Err(e) = file.write_all(&bytes).await {
                            let _ = tx.send(ProgressMessage {
                                id: *id,
                                state: Some(DownloadState::Failed(format!("Errore scrittura: {e}"))),
                                current,
                                total,
                            }).await;
                            return Attempt::FailedAlreadyReported;
                        }
                        current += bytes.len() as u64;
                        if current - last_sent >= THROTTLE_BYTES {
                            last_sent = current;
                            let _ = tx.send(ProgressMessage { id: *id, state: None, current, total }).await;
                        }
                    }
                    Some(Err(_)) => {
                        // Flush obbligatorio: `current` deve riflettere i byte REALMENTE su disco
                        // (invariante per append recovery e Range: bytes={current}-)
                        let _ = file.flush().await;
                        return Attempt::Disconnected { current };
                    }
                    None => {
                        let _ = file.flush().await;
                        let _ = tx.send(ProgressMessage {
                            id: *id,
                            state: Some(DownloadState::Completed),
                            current,
                            total,
                        }).await;
                        return Attempt::Completed;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::types::{DownloadState, ProgressMessage};
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const BODY: &[u8] = b"0123456789abcdef0123456789abcdef"; // 32 byte

    async fn setup_server(body: &[u8]) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file.bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body.to_vec())
                    .insert_header("content-length", body.len().to_string()),
            )
            .mount(&server)
            .await;
        server
    }

    async fn recv_state(rx: &mut mpsc::Receiver<ProgressMessage>) -> DownloadState {
        loop {
            let msg = rx.recv().await.expect("canale chiuso senza Completed");
            if let Some(state) = msg.state {
                return state;
            }
        }
    }

    #[tokio::test]
    async fn download_completed_writes_file() {
        let server = setup_server(BODY).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let url = format!("{}/file.bin", server.uri());

        let (tx, mut rx) = mpsc::channel(100);
        let _ctl = spawn_download(1, url, None, dest.clone(), tx);

        let state = recv_state(&mut rx).await;
        assert_eq!(state, DownloadState::Completed);
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), BODY);
    }
}
