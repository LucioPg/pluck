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

/// allow(dead_code) temporaneo: url/referer per tooltip/retry futuri
#[derive(Clone, Debug, PartialEq)]
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
    /// Thumbnail del candidato sorgente (None per download manuali)
    pub thumbnail: Option<String>,
    /// true = stream HLS: current/total in SECONDI, niente pausa
    pub is_hls: bool,
}

impl DownloadItem {
    pub fn percentage(&self) -> u64 {
        (self.current * 100).checked_div(self.total).unwrap_or(0)
    }
}

/// Secondi → `mm:ss` o `h:mm:ss` (progresso HLS, current/total in secondi).
pub fn format_time(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
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

/// Handle restituito alla UI per comandare un download.
pub struct DownloadControl {
    pub cancel: CancellationToken,
    /// UI scrive true (pausa) / false (resume); il Receiver resta interno al task
    pub pause: watch::Sender<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_item(current: u64, total: u64) -> DownloadItem {
        DownloadItem {
            id: 1,
            url: "https://esempio.com/file".into(),
            referer: None,
            filename: PathBuf::from("file"),
            state: DownloadState::Downloading,
            current,
            total,
            thumbnail: None,
            is_hls: false,
        }
    }

    #[test]
    fn percentage_basic() {
        let item = test_item(50, 200);
        assert_eq!(item.percentage(), 25);
    }

    #[test]
    fn percentage_zero_total_is_zero() {
        let item = test_item(10, 0);
        assert_eq!(item.percentage(), 0);
    }

    #[test]
    fn percentage_full() {
        let item = test_item(200, 200);
        assert_eq!(item.percentage(), 100);
    }

    #[test]
    fn format_time_units() {
        assert_eq!(format_time(0), "00:00");
        assert_eq!(format_time(65), "01:05");
        assert_eq!(format_time(3600), "1:00:00");
        assert_eq!(format_time(7385), "2:03:05");
    }
}
