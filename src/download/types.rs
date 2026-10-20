//! Tipi dominio del motore download. Nessuna dipendenza da Dioxus/UI.

use std::path::PathBuf;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub type DownloadId = u64;

#[derive(Clone, Debug, PartialEq)]
pub enum DownloadState {
    Downloading,
    Paused,
    Completed,
    Failed(String),
    Cancelled,
}

/// url/referer conservati per la card/tooltip e futuro retry — evita dead_code sotto clippy -D
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct DownloadItem {
    pub id: DownloadId,
    pub url: String,
    pub referer: Option<String>,
    pub filename: PathBuf,
    pub state: DownloadState,
    pub current: u64,
    /// 0 = Content-Length assente → barra indeterminata
    pub total: u64,
}

/// Messaggio di progresso dal motore alla UI.
#[derive(Clone, Debug)]
pub struct ProgressMessage {
    pub id: DownloadId,
    /// Some = cambio stato; None = solo avanzamento
    pub state: Option<DownloadState>,
    pub current: u64,
    pub total: u64,
}

impl ProgressMessage {
    pub fn percentage(&self) -> u64 {
        if self.total == 0 {
            0
        } else {
            (self.current * 100) / self.total
        }
    }
}

/// Handle restituito alla UI per comandare un download.
pub struct DownloadControl {
    pub cancel: CancellationToken,
    /// UI scrive true (pausa) / false (resume); il Receiver resta interno al task
    pub pause: watch::Sender<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_basic() {
        let msg = ProgressMessage { id: 1, state: None, current: 50, total: 200 };
        assert_eq!(msg.percentage(), 25);
    }

    #[test]
    fn percentage_zero_total_is_zero() {
        let msg = ProgressMessage { id: 1, state: None, current: 10, total: 0 };
        assert_eq!(msg.percentage(), 0);
    }

    #[test]
    fn percentage_full() {
        let msg = ProgressMessage { id: 1, state: None, current: 200, total: 200 };
        assert_eq!(msg.percentage(), 100);
    }
}
