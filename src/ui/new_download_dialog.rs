use dioxus::prelude::*;
use std::path::PathBuf;

#[component]
pub fn NewDownloadDialog(
    mut open: Signal<bool>,
    /// Spoofing referrer (radio nell'header): false → campo referer disabilitato,
    /// nessun Referer sul download
    spoofing: Signal<bool>,
    /// Referer preferiti salvati (per il dropdown)
    referers: Vec<String>,
    /// (url, referer, destinazione) — chiamato solo se validazione OK
    on_submit: EventHandler<(String, Option<String>, PathBuf)>,
    /// riservato: rfd save_file non espone errori (None = annullato); hook per toast futuri
    on_error: EventHandler<String>,
    /// Salva il contenuto del campo referer come preferito (senza avviare il download)
    on_save_referer: EventHandler<String>,
    /// Elimina il referer indicato dai preferiti
    on_remove_referer: EventHandler<String>,
) -> Element {
    // Prop riservata ai toast futuri: rfd non espone errori di save_file.
    let _ = on_error;

    let mut url = use_signal(String::new);
    let mut referer = use_signal(String::new);
    let mut filename = use_signal(String::new);
    let mut browsing = use_signal(|| false);
    // Dropdown referer: DaisyUI 5 mostra il content via :focus-within (resta aperto
    // dopo il click su una voce). `dropdown-close` forzato a segnale false vince
    // sul focus e garantisce la chiusura esplicita alla selezione.
    let mut dropdown_open = use_signal(|| false);

    let url_ok = reqwest::Url::parse(&url.read()).is_ok();
    let filename_ok = !filename.read().trim().is_empty();
    let can_start = url_ok && filename_ok;
    let spoof_on = *spoofing.read();

    let browse = move |_| {
        // guard anti doppio-click: il disabled arriva solo al prossimo render
        if *browsing.read() {
            return;
        }
        // Primo filtro = preselezionato dal dialog; "Tutti i file" permette qualsiasi tipo.
        let mut dialog = rfd::AsyncFileDialog::new()
            .add_filter("MP4", &["mp4"])
            .add_filter("All files", &["*"]);
        let suggested = filename.read().trim().to_string();
        if !suggested.is_empty() {
            dialog = dialog.set_file_name(&suggested);
        }
        browsing.set(true);
        spawn(async move {
            match dialog.save_file().await {
                Some(handle) => {
                    // rfd FileHandle::path() → std::path::PathBuf
                    if let Some(path) = handle.path().to_str() {
                        filename.set(path.to_string());
                    }
                }
                None => { /* utente ha annullato: nessun errore */ }
            }
            browsing.set(false);
        });
    };

    rsx! {
        div { class: "modal modal-open",
            div { class: "modal-box",
                h3 { class: "font-bold text-lg mb-4", "New download" }
                div { class: "flex flex-col gap-3",
                    fieldset { class: "fieldset",
                        legend { class: "fieldset-legend", "URL" }
                        input {
                            class: "input w-full",
                            r#type: "url",
                            placeholder: "https://example.com/file.mp4",
                            value: "{url}",
                            oninput: move |e| url.set(e.value()),
                        }
                        if !url.read().is_empty() && !url_ok {
                            p { class: "label text-error", "Invalid URL" }
                        }
                    }
                    fieldset { class: "fieldset",
                        legend { class: "fieldset-legend", "Referer (optional)" }
                        input {
                            class: "input w-full",
                            r#type: "text",
                            placeholder: "https://example.com",
                            value: "{referer}",
                            disabled: !spoof_on,
                            oninput: move |e| referer.set(e.value()),
                        }
                        if !spoof_on {
                            p { class: "label text-base-content/60",
                                "Spoofing off: no Referer will be sent"
                            }
                        }
                        div { class: "flex items-center gap-2 mt-2",
                            button {
                                class: "btn btn-xs",
                                onclick: move |_| {
                                    let r = referer.read().trim().to_string();
                                    if !r.is_empty() {
                                        on_save_referer(r);
                                    }
                                },
                                "Save referer"
                            }
                            if !referers.is_empty() {
                                div {
                                    class: if *dropdown_open.read() {
                                        "dropdown"
                                    } else {
                                        "dropdown dropdown-close"
                                    },
                                    div {
                                        tabindex: "0",
                                        role: "button",
                                        class: "btn btn-xs",
                                        onclick: move |_| dropdown_open.set(true),
                                        "Saved referers ({referers.len()}) ▾"
                                    }
                                    // Niente classe `menu` DaisyUI: le sue regole
                                    // (flex-flow column wrap + display grid sui
                                    // figli) sfondano il pannello con URL lunghi
                                    // (voci in colonne affiancate). Flex puro.
                                    ul { class: "dropdown-content bg-base-200 rounded-box z-50 w-80 flex flex-col gap-1 p-2 shadow-lg",
                                        // Due closure `move` per voce → due copie di `r`
                                        // (una closure consumerebbe `r` dell'altra).
                                        for (r, r_del) in referers.iter().cloned().map(|r| (r.clone(), r)) {
                                            li { class: "flex items-center justify-between gap-2",
                                                key: "{r}",
                                                button {
                                                    class: "min-w-0 flex-1 truncate rounded-md px-3 py-2 text-sm text-left hover:bg-base-300",
                                                    onclick: move |_| {
                                                        referer.set(r.clone());
                                                        dropdown_open.set(false);
                                                    },
                                                    "{r}"
                                                }
                                                button {
                                                    class: "btn btn-ghost btn-xs shrink-0",
                                                    aria_label: "Delete referer",
                                                    onclick: move |_| on_remove_referer(r_del.clone()),
                                                    "✕"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    fieldset { class: "fieldset",
                        legend { class: "fieldset-legend", "File di destinazione" }
                        div { class: "flex gap-2",
                            input {
                                class: "input w-full",
                                r#type: "text",
                                placeholder: "C:\\path\\file.mp4",
                                value: "{filename}",
                                oninput: move |e| filename.set(e.value()),
                            }
                            button {
                                class: "btn",
                                disabled: *browsing.read(),
                                onclick: browse,
                                if *browsing.read() { span { class: "loading loading-spinner loading-xs" } }
                                "Browse…"
                            }
                        }
                        if !filename_ok {
                            p { class: "label text-error", "Choose a destination file" }
                        }
                    }
                }
                div { class: "modal-action",
                    button { class: "btn", onclick: move |_| open.set(false), "Cancel" }
                    button {
                        class: "btn btn-primary",
                        disabled: !can_start || *browsing.read(),
                        onclick: move |_| {
                            // Spoofing OFF → il referer non viene inviato né salvato
                            let referer_val = if spoof_on {
                                referer.read().trim().to_string()
                            } else {
                                String::new()
                            };
                            on_submit((
                                url.read().clone(),
                                if referer_val.is_empty() { None } else { Some(referer_val) },
                                PathBuf::from(filename.read().trim()),
                            ));
                            url.set(String::new());
                            referer.set(String::new());
                            filename.set(String::new());
                        },
                        "Start"
                    }
                }
            }
            // click sul backdrop chiude
            div { class: "modal-backdrop", onclick: move |_| open.set(false) }
        }
    }
}
