//! Thumbnail quadrata con fallback placeholder — condivisa da candidati e
//! lista download. Nessuna rounded: box quadrata piena (scelta utente).
//!
//! L'immagine parte dall'URL originale (hotlink diretto); onerror (403 di
//! hotlink-protection, URL morto) → placeholder SVG inline, senza retry.

use dioxus::prelude::*;

/// Icona film-strip placeholder (stroke grigio neutro, tono base-content).
const PLACEHOLDER: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='%23a6adbb' stroke-width='1.5'%3E%3Crect x='3' y='4' width='18' height='16'/%3E%3Cpath d='M7 4v16M17 4v16M3 9h4M3 15h4M17 9h4M17 15h4'/%3E%3C/svg%3E";

#[component]
pub fn Thumb(
    /// URL della thumbnail; None o caricamento fallito → placeholder
    url: Option<String>,
) -> Element {
    let mut broken = use_signal(|| false);
    let show_img = url.as_ref().is_some_and(|u| !u.is_empty()) && !*broken.read();
    rsx! {
        div { class: "w-16 h-16 shrink-0 bg-base-300 overflow-hidden",
            if show_img {
                img {
                    src: url.clone().unwrap(),
                    class: "w-full h-full object-cover",
                    alt: "Thumbnail",
                    onerror: move |_| broken.set(true),
                }
            } else {
                img { src: PLACEHOLDER, class: "w-full h-full", alt: "" }
            }
        }
    }
}
