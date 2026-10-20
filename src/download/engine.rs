//! Motore download. SOLO reqwest/tokio/tokio-util — zero import UI (vincolo addon futuro).

use crate::download::types::*;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Invia progress solo ogni THROTTLE_BYTES scaricati (riduce rumore sul canale).
const THROTTLE_BYTES: u64 = 64 * 1024;

/// Da chiamare dentro un runtime tokio (tokio::spawn panica altrimenti) — UI e futuro bridge addon.
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
                Attempt::Disconnected { current: newer } => {
                    let _ = tx.send(ProgressMessage {
                        id,
                        state: Some(DownloadState::Failed(
                            "Connessione persa, recovery fallita".into(),
                        )),
                        current: newer,
                        total: 0,
                    }).await;
                }
                Attempt::FailedAlreadyReported => {}
            }
        }
        Attempt::Disconnected { .. } => {
            let _ = tx.send(ProgressMessage {
                id,
                state: Some(DownloadState::Failed(
                    "Connessione persa o URL non raggiungibile".into(),
                )),
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
    // Timeout client: connect limita l'handshake TCP/TLS, read_timeout è idle tra
    // read del body (download lunghi legittimi restano possibili)
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .expect("client");
    let mut request = client.get(url);
    if let Some(r) = referer {
        request = request.header("Referer", r);
    }
    if offset > 0 {
        request = request.header("Range", format!("bytes={offset}-"));
    }

    // send cancel-aware: il bottone Annulla deve funzionare anche durante l'handshake
    let response = tokio::select! {
        r = request.send() => match r {
            Ok(resp) => resp,
            Err(_) => return Attempt::Disconnected { current: offset },
        },
        _ = cancel.cancelled() => {
            // In recovery (offset > 0) il file parziale del primo tentativo esiste
            // ed è già flushato → va rimosso prima di segnalare il cancel
            if offset > 0 {
                let _ = tokio::fs::remove_file(dest).await;
            }
            let _ = tx.send(ProgressMessage {
                id: *id,
                state: Some(DownloadState::Cancelled),
                current: 0,
                total: 0,
            }).await;
            return Attempt::Cancelled;
        }
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

    // 206 non conforme: appendere su una base diversa da offset corromperebbe il file
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let start_ok = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split('-').next())
            .and_then(|s| s.strip_prefix("bytes="))
            .and_then(|s| s.trim().parse::<u64>().ok())
            .is_some_and(|start| start == offset);
        if !start_ok {
            let _ = tx.send(ProgressMessage {
                id: *id,
                state: Some(DownloadState::Failed(format!(
                    "Server non conforme: Content-Range non inizia a {offset}"
                ))),
                current: offset,
                total: 0,
            }).await;
            return Attempt::FailedAlreadyReported;
        }
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

    // open cancel-aware (open è atomico: nessun file parziale intermedio)
    let opened = {
        let mut opts = OpenOptions::new();
        opts.create(true)
            .write(true)
            .append(current > 0) // recovery 206: append da EOF (== current: mai scritto oltre current)
            .truncate(current == 0);
        tokio::select! {
            r = opts.open(dest) => r,
            _ = cancel.cancelled() => {
                // Coerenza con il braccio send-cancel: cancel = niente file parziale
                if offset > 0 {
                    let _ = tokio::fs::remove_file(dest).await;
                }
                let _ = tx.send(ProgressMessage {
                    id: *id,
                    state: Some(DownloadState::Cancelled),
                    current: 0,
                    total: 0,
                }).await;
                return Attempt::Cancelled;
            }
        }
    };
    let mut file = match opened {
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
        // Check pausa a INIZIO loop: una pausa arrivata PRIMA del primo select! rende
        // pause_rx.changed() subito pronto → continue; senza questo check il download
        // continuerebbe mentre risulta in pausa.
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

    #[tokio::test]
    async fn referer_header_is_sent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file.bin"))
            .and(wiremock::matchers::header("referer", "https://esempio.com"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(BODY.to_vec())
                    .insert_header("content-length", BODY.len().to_string()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let url = format!("{}/file.bin", server.uri());

        let (tx, mut rx) = mpsc::channel(100);
        let _ctl = spawn_download(2, url, Some("https://esempio.com".into()), dest, tx);

        let state = recv_state(&mut rx).await;
        assert_eq!(state, DownloadState::Completed);
        // .expect(1) su mock non matchato → wiremock verifica al drop del server (panic se 0 match)
    }

    #[tokio::test]
    async fn cancel_deletes_partial_file() {
        // Server raw che invia head + primo blocco del body e poi RESTA in attesa
        // (socket aperto, nessun dato): il motore scrive su disco e si blocca sul
        // chunk successivo → il cancel arriva nel braccio del loop, non in send().
        // (spawn_raw_server qui non basta: chiude lo socket dopo ogni risposta.)
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
        let _server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Leggi la richiesta fino a fine head (il GET non ha body)
            let mut buf = Vec::with_capacity(8192);
            let mut chunk = [0u8; 2048];
            loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000000\r\n\r\n")
                .await;
            let _ = sock.write_all(&vec![0u8; 100_000]).await;
            // Stall: la connessione resta aperta senza inviare altro
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");

        let (tx, mut rx) = mpsc::channel(100);
        let ctl = spawn_download(3, url, None, dest.clone(), tx);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            dest.exists(),
            "il file parziale deve esistere PRIMA del cancel"
        );
        ctl.cancel.cancel();

        let state = tokio::time::timeout(std::time::Duration::from_secs(5), recv_state(&mut rx))
            .await
            .expect("timeout in attesa di Cancelled");
        assert_eq!(state, DownloadState::Cancelled);
        assert!(!dest.exists(), "il file parziale deve essere cancellato");
    }

    #[tokio::test]
    async fn cancel_during_handshake_reports_cancelled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file.bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(BODY.to_vec())
                    .insert_header("content-length", BODY.len().to_string())
                    .set_delay(std::time::Duration::from_secs(30)),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let url = format!("{}/file.bin", server.uri());

        let (tx, mut rx) = mpsc::channel(100);
        let ctl = spawn_download(7, url, None, dest.clone(), tx);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await; // send() in attesa risposta
        ctl.cancel.cancel();

        let state = tokio::time::timeout(std::time::Duration::from_secs(3), recv_state(&mut rx))
            .await
            .expect("timeout: cancel durante handshake non effettivo");
        assert_eq!(state, DownloadState::Cancelled);
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn pause_then_resume_completes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/file.bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(vec![7u8; 500_000])
                    .insert_header("content-length", "500000")
                    .set_delay(std::time::Duration::from_secs(1)),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");
        let url = format!("{}/file.bin", server.uri());

        let (tx, mut rx) = mpsc::channel(100);
        let ctl = spawn_download(4, url, None, dest.clone(), tx);

        // Pausa immediata: il task entra in pausa prima di leggere il body
        ctl.pause.send(true).unwrap();
        let state = tokio::time::timeout(std::time::Duration::from_secs(3), recv_state(&mut rx))
            .await
            .expect("timeout in attesa di Paused");
        assert_eq!(state, DownloadState::Paused);

        // Resume dopo la finestra di pausa: il motore riparte e completa
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        ctl.pause.send(false).unwrap();

        let state = tokio::time::timeout(std::time::Duration::from_secs(10), recv_state(&mut rx))
            .await
            .expect("timeout in attesa di Completed dopo resume");
        assert_eq!(state, DownloadState::Completed);
        assert_eq!(tokio::fs::read(&dest).await.unwrap().len(), 500_000);
    }

    /// Server TCP "grezzo" per i test di recovery: risponde con le raw response in
    /// ordine (una per connessione) e chiude lo stream. Necessario al posto di
    /// wiremock perché il troncamento va simulato per davvero: wiremock incornicia
    /// sempre il body completo (Full<Bytes>) e con content-length bugiardo hyper
    /// 1.12 lato server, in debug, ASSERTA il mismatch invece di produrre uno
    /// stream incompleto. Restituisce (uri, handle) con le richieste ricevute.
    async fn spawn_raw_server(
        raw_responses: Vec<Vec<u8>>,
    ) -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(raw_responses.len());
            for resp in raw_responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                // Leggi la richiesta fino a fine head (il GET non ha body)
                let mut buf = Vec::with_capacity(8192);
                let mut chunk = [0u8; 2048];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                requests.push(buf);
                // Scrivi e chiudi. Sulla risposta troncata la chiusura arriva PRIMA
                // del content-length → errore di stream lato client.
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, &resp).await;
            }
            requests
        });
        (uri, handle)
    }

    /// Prima risposta 200: head con content-length pieno ma body troncato a 16 byte
    fn truncated_first_response() -> Vec<u8> {
        let mut raw = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", BODY.len())
            .into_bytes();
        raw.extend_from_slice(&BODY[..16]);
        raw
    }

    #[tokio::test]
    async fn recovery_range_206_appends() {
        // 2ª risposta (recovery con Range): 206 con la parte restante
        let rest = &BODY[16..];
        let mut second = format!(
            "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes=16-31/32\r\ncontent-length: {}\r\n\r\n",
            rest.len()
        )
        .into_bytes();
        second.extend_from_slice(rest);

        let (url, server) = spawn_raw_server(vec![truncated_first_response(), second]).await;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");

        let (tx, mut rx) = mpsc::channel(100);
        let _ctl = spawn_download(5, url, None, dest.clone(), tx);

        let state = tokio::time::timeout(std::time::Duration::from_secs(5), recv_state(&mut rx))
            .await
            .expect("timeout: nessun Completed dopo recovery");
        assert_eq!(state, DownloadState::Completed);
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), BODY);

        // Il recovery deve aver mandato Range: bytes=16- (equivalente del matcher
        // header_regex della versione wiremock del piano)
        let requests = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("timeout: server raw non ha chiuso")
            .unwrap();
        assert_eq!(requests.len(), 2);
        let first_req = String::from_utf8_lossy(&requests[0]).to_ascii_lowercase();
        assert!(
            !first_req.contains("range:"),
            "la prima richiesta non deve avere Range: {first_req}"
        );
        let second_req = String::from_utf8_lossy(&requests[1]).to_ascii_lowercase();
        assert!(
            second_req.contains("range: bytes=16-"),
            "la richiesta di recovery non contiene Range: bytes=16-: {second_req}"
        );
    }

    #[tokio::test]
    async fn recovery_without_range_restarts_from_zero() {
        // 2ª risposta: 200 COMPLETO (server che ignora Range) → restart da zero
        let mut second = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", BODY.len())
            .into_bytes();
        second.extend_from_slice(BODY);

        let (url, server) = spawn_raw_server(vec![truncated_first_response(), second]).await;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.bin");

        let (tx, mut rx) = mpsc::channel(100);
        let _ctl = spawn_download(6, url, None, dest.clone(), tx);

        let state = tokio::time::timeout(std::time::Duration::from_secs(5), recv_state(&mut rx))
            .await
            .expect("timeout: nessun Completed dopo recovery no-Range");
        assert_eq!(state, DownloadState::Completed);
        // BODY esatto: con append sbagliato sarebbero 16+32=48
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), BODY);

        let requests = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("timeout: server raw non ha chiuso")
            .unwrap();
        assert_eq!(requests.len(), 2);
        let first_req = String::from_utf8_lossy(&requests[0]).to_ascii_lowercase();
        assert!(
            !first_req.contains("range:"),
            "la prima richiesta non deve avere Range: {first_req}"
        );
    }
}
