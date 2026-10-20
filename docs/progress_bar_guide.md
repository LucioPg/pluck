# Sistema Progress Bar — Guida per il porting

Estrazione del sistema di progress tracking di PWDManager, riutilizzabile in un altro progetto
Dioxus 0.7 + tokio. Tre layer: **tipi condivisi**, **backend che invia messaggi**, **UI che li consuma**.

---

## 1. Architettura

```
┌─────────────────────────────┐
│ Backend (pipeline async)    │
│ Option<Arc<ProgressSender>> │──┐
└─────────────────────────────┘  │  tokio::sync::mpsc
                                 │  channel::<ProgressMessage>(100)
┌─────────────────────────────┐  │
│ Dioxus component            │  │
│  use_effect                 │  │
│   ├─ spawn: receiver loop   │◄─┘  rx.recv() → Signal
│   └─ spawn: worker task     │      (stage, %, messaggio)
└─────────────────────────────┘
```

File originali:
- `src/backend/migration_types.rs` — tipi
- `src/backend/password_utils.rs` / `src/backend/export.rs` — invio dal backend
- `src/components/features/progress_migration.rs` — componente UI
- `src/components/globals/dialogs/migration_progress.rs` — wrapper in modal

Dipendenze: `tokio` (feature `sync` per mpsc), `dioxus 0.7`, **DaisyUI 5** (solo classi CSS
`progress progress-primary` — nessun CSS custom necessario).

---

## 2. Tipi (`migration_types.rs`)

```rust
use tokio::sync::mpsc::Sender;

/// Stage corrente dell'operazione lunga.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum MigrationStage {
    #[default]
    Idle,
    Decrypting,
    Encrypting,
    Serializing,   // Serializzazione per export
    Deserializing, // Parsing per import (JSON/CSV/XML)
    Reading,       // Lettura file per import
    Writing,       // Scrittura file
    Deduplicating, // Rimozione duplicati per import
    Importing,     // Salvataggio nel DB per import
    Finalizing,
    Completed,
    Failed,
}

/// Messaggio di progresso inviato tramite canale mpsc.
#[derive(Clone, Debug)]
pub struct ProgressMessage {
    pub stage: MigrationStage,
    pub current: usize,
    pub total: usize,
}

impl ProgressMessage {
    pub fn new(stage: MigrationStage, current: usize, total: usize) -> Self {
        Self { stage, current, total }
    }

    /// Percentuale di completamento (0-100).
    pub fn percentage(&self) -> usize {
        if self.total == 0 {
            0
        } else {
            (self.current * 100) / self.total
        }
    }
}

/// Type alias per il sender del progresso.
pub type ProgressSender = Sender<ProgressMessage>;
```

Note:
- `MigrationStage` è un enum **generico di dominio**, non legato a una barra: adattare le varianti
  al proprio dominio (es. `Downloading`, `Compiling`, ...).
- `percentage()` arrotonda per difetto e **non clampa** sopra 100 (responsabilità del caller).
- `total == 0` → 0% (evita divisione per zero).

---

## 3. Pattern backend (mittente)

La pipeline riceve `Option<Arc<ProgressSender>>` — `None` = esecuzione silenziosa, così la stessa
funzione è riutilizzabile con o senza UI.

```rust
pub async fn my_pipeline_with_progress(
    /* ... */
    progress_tx: Option<Arc<ProgressSender>>,
) -> Result<(), String> {
    // 1. Cambio stage manuale tra le fasi
    if let Some(tx) = &progress_tx {
        let _ = tx.send(ProgressMessage::new(MigrationStage::Serializing, 0, total)).await;
    }

    // 2. Progresso per-item dentro un loop
    for (i, item) in items.iter().enumerate() {
        process(item).await?;
        if let Some(tx) = &progress_tx {
            let _ = tx.send(ProgressMessage::new(MigrationStage::Processing, i + 1, total)).await;
        }
    }

    // 3. Completamento
    if let Some(tx) = &progress_tx {
        let _ = tx.send(ProgressMessage::new(MigrationStage::Completed, total, total)).await;
    }
    Ok(())
}
```

### Variante parallela (rayon dentro `spawn_blocking`)

Se il lavoro per-item è CPU-bound e parallelo (`into_par_iter`), **usare `try_send`** +
`AtomicUsize` per il contatore, perché si è in contesto sincrono non-async:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::task;

let total = items.len();
let completed = Arc::new(AtomicUsize::new(0));
let progress_tx_clone = progress_tx.clone();

let result = task::spawn_blocking(move || {
    items
        .into_par_iter()
        .map(|item| {
            // ... lavoro CPU-bound ...

            // Aggiorna progress
            if let Some(tx) = &progress_tx_clone {
                let current = completed.fetch_add(1, Ordering::SeqCst) + 1;
                // try_send invece di blocking_send per evitare deadlock
                let _ = tx.try_send(ProgressMessage::new(
                    MigrationStage::Decrypting,
                    current,
                    total,
                ));
            }
            Ok(processed_item)
        })
        .collect::<Result<Vec<_>, MyError>>()
});
```

Regole invio:
- `send(...).await` in contesto async; `try_send(...)` in contesto sync/parallelo (mai deadlock).
- Ignorare l'errore di invio con `let _ =` — il receiver può essere già chiuso (UI smontata).
- Inviare `Completed` come messaggio, non solo impostandolo lato UI.

---

## 4. Componente UI Dioxus (`ProgressMigrationChn`)

Il componente **avvia il lavoro automaticamente al mount** (via `use_effect`) e disegna
messaggio + barra + percentuale.

```rust
use dioxus::prelude::*;
use std::sync::Arc;
use tokio::sync::mpsc;

#[allow(non_snake_case)]
#[component]
pub fn ProgressChn(
    /// Signal true quando completato
    on_completed: Signal<bool>,
    /// Signal true se fallisce
    on_failed: Signal<bool>,
) -> Element {
    let mut stage = use_signal(|| MigrationStage::Idle);
    let mut progress = use_signal(|| 0usize);
    let mut status_message = use_signal(String::new);
    let mut started = use_signal(|| false); // evita doppio avvio

    use_effect(move || {
        if started() {
            return;
        }
        started.set(true);

        let (tx, mut rx) = mpsc::channel::<ProgressMessage>(100);

        // Task 1: receiver — aggiorna i Signal
        spawn(async move {
            while let Some(msg) = rx.recv().await {
                stage.set(msg.stage.clone());
                progress.set(msg.percentage());
                status_message.set(format_stage_message(&msg.stage));

                if msg.stage == MigrationStage::Completed {
                    on_completed.set(true);
                }
            }
        });

        // Task 2: worker — esegue il lavoro passando tx al backend
        spawn(async move {
            match my_pipeline_with_progress(Some(Arc::new(tx))).await {
                Ok(()) => {} // il receiver vede Completed
                Err(e) => {
                    status_message.set(format!("Operation failed: {e}"));
                    stage.set(MigrationStage::Failed);
                    on_failed.set(true);
                }
            }
        });
    });

    rsx! {
        div { class: "flex flex-col gap-4 w-full",
            // Messaggio stato
            p { class: "text-center font-medium text-base-content", "{status_message}" }

            // Progress bar DaisyUI
            progress {
                class: "progress progress-primary w-full",
                value: "{progress}",
                max: "100",
            }

            // Percentuale
            p { class: "text-center text-sm opacity-70", "{progress}%" }
        }
    }
}

fn format_stage_message(stage: &MigrationStage) -> String {
    match stage {
        MigrationStage::Idle => "Preparing...".to_string(),
        MigrationStage::Decrypting => "Decrypting passwords...".to_string(),
        MigrationStage::Completed => "Completed!".to_string(),
        MigrationStage::Failed => "Operation failed".to_string(),
        _ => "Processing...".to_string(),
    }
}
```

### Punti chiave del componente

| Punto | Dettaglio |
|---|---|
| Flag `started` | `use_effect` gira a ogni re-render del parent; il flag impedisce doppi avvi della pipeline |
| Due `spawn` separati | Receiver e worker sono indipendenti: la UI resta reattiva mentre il lavoro procede |
| Buffer canale 100 | Assorbe burst di messaggi; il receiver li consuma a ritmo di render |
| `while let Some(msg) = rx.recv()` | Quando il worker drop `tx`, `recv()` ritorna `None` e il task receiver termina da solo |
| `stage.set(msg.stage.clone())` | Clone necessario: `stage` è posseduto dal Signal; oppure rendere l'enum `Copy` se senza dati |
| Fine con errore | Su `Err` il worker imposta `Failed` direttamente (il receiver non riceve nulla di utile dopo) |

---

## 5. Wrapper in modal (opzionale)

Dialog sottile che mostra la barra dentro un modal, con avviso di non interrompere l'operazione:

```rust
#[component]
pub fn ProgressDialog(
    open: Signal<bool>,
    on_completed: Signal<bool>,
    #[props(default)] on_failed: Signal<bool>,
    #[props(default)] on_cancel: EventHandler<()>,
) -> Element {
    rsx! {
        BaseModal {
            open,
            on_close: move |_| {}, // chiusura programmatica disabilitata durante il lavoro
            h3 { class: "font-bold text-lg mb-2", "Operation in progress" }
            p { class: "text-warning py-2",
                strong { "Attention: " }
                "Do not close the app until the process is completed!"
            }
            ProgressChn { on_completed, on_failed }
        }
    }
}
```

Il parent osserva `on_completed` / `on_failed` (es. con `use_effect` o nei propri task) per
chiudere il modal e navigare avanti.

---

## 6. Checklist porting

> Setup ambiente (npm, Cargo, CSS) → vedere §8.

1. Copiare/adattare `MigrationStage`, `ProgressMessage`, `ProgressSender` → verificare test `percentage()`
2. Aggiungere parametro `Option<Arc<ProgressSender>>` alla pipeline + invii di stage/per-item
3. Creare componente UI (canale 100, due `spawn`, flag anti-doppio-avvio)
4. Classi CSS richieste (DaisyUI 5 + Tailwind): `progress`, `progress-primary`, `w-full`,
   `flex flex-col gap-4`, `text-center font-medium`, `text-sm opacity-70`
5. Wire-up: parent crea `Signal<bool>` per `on_completed`/`on_failed`, reagisce a fine lavoro

## 7. Gotchas incontrati nel progetto originale

- **`try_send` vs `blocking_send`**: dentro `spawn_blocking` + `par_iter`, `blocking_send` può
  deadlockare; `try_send` scarta messaggi se il buffer è pieno (accettabile per una barra: il
  prossimo messaggio corregge il valore).
- **`percentage()` > 100**: nessun clamp — se `current` può superare `total` (race in parallelo),
  clampare lato UI.
- **Indice parallelo**: `fetch_add` + 1 dà un contatore progressivo anche se l'ordine di
  completamento non è deterministico.
- **Channel chiuso**: tutti gli invii usano `let _ =` — mai `unwrap`, la UI può essere sparita.

---

## 8. Installazione / integrazione in un nuovo progetto Dioxus

Setup di riferimento nel progetto originale: **Dioxus 0.7.x (desktop)**, **Tailwind CSS v4**,
**DaisyUI 5**, tokio 1.49. La barra non richiede nessun crate JS di runtime — solo build-time CSS.

### 8.1 Dipendenze Rust (`Cargo.toml`)

Minime indispensabili per il sistema progress:

```toml
[dependencies]
dioxus = { version = "0.7", features = ["desktop"] }
# sync = canale mpsc, rt = spawn_blocking (serve solo per la variante parallela)
tokio = { version = "1", default-features = false, features = ["sync", "rt"] }

# SOLO per la variante parallela (par_iter in spawn_blocking)
rayon = "1.11"
```

Note:
- `spawn` (da `dioxus::prelude`) gira sul runtime tokio interno di Dioxus — non serve `#[tokio::main]`
  né un runtime proprio in un'app desktop Dioxus.
- Se il progetto ha già `tokio` con `features = ["full"]`, non aggiungere nulla.
- Il crate con i tipi (`MigrationStage`/`ProgressMessage`) può stare in `src/backend/` del progetto
  oppure in una lib separata se più progetti lo condividono.

### 8.2 Setup CSS (Tailwind v4 + DaisyUI 5)

**Installazione pacchetti npm** (solo devDependencies, build-time):

```bash
npm install -D tailwindcss @tailwindcss/cli daisyui
```

**File CSS sorgente** (es. `assets/input.css`) — minimo per la progress bar:

```css
@import "tailwindcss";
@plugin "daisyui";

/* Fa scansionare a Tailwind le classi usate nel RSX — indispensabile */
@source "../src/**/*.rs";
```

**Build CSS**:

```bash
npx @tailwindcss/cli -i ./assets/input.css -o ./assets/tailwind.css
# watch durante lo sviluppo:
npx @tailwindcss/cli -i ./assets/input.css -o ./assets/tailwind.css --watch
```

**Variante PostCSS** (quella usata dal progetto originale): invece di `@tailwindcss/cli`,
installare `postcss postcss-cli @tailwindcss/postcss` e creare `postcss.config.js` nella root:

```js
module.exports = {
  plugins: {
    '@tailwindcss/postcss': {},
  },
}
```

Poi la build passa per `npx postcss`:

```json
{
  "scripts": {
    "build:css": "npx postcss ./assets/input.css -o ./assets/tailwind.css",
    "watch:css": "npx postcss ./assets/input.css -o ./assets/tailwind.css --watch"
  }
}
```

**Pipeline del progetto originale** — due file CSS separati, due output:

```
assets/input.css       (Tailwind: @import, @plugin daisyui, @source)  → assets/tailwind.css
assets/input_main.css  (design system custom: classi .pwd-*, @utility) → assets/main.css
```

Entrambi gli output vengono poi embeddati in `main.rs` con `include_str!` (vedi §8.3).
Per il solo progress bar serve solo il primo file; il secondo esiste perché tutto il resto
dell'app usa classi custom (convenzione `pwd-*`).

⚠️ Il vecchio comando `npx tailwindcss -i -o` è sintassi Tailwind **v3**: non funziona con
questa config v4.

**Classi necessarie alla barra** (verificare che escano nel CSS compilato):

| Classe | Origine |
|---|---|
| `progress`, `progress-primary` | DaisyUI 5 (component + variant) |
| `w-full`, `flex`, `flex-col`, `gap-4` | Tailwind utilities |
| `text-center`, `font-medium`, `text-sm`, `opacity-70` | Tailwind utilities |

Con `@source` puntato ai `.rs`, Tailwind le genera da solo solo se presenti nel codice sorgente —
se una classe manca nel CSS compilato, controllare che il componente sia effettivamente incluso
nella build (file raggiungibile da `@source`).

### 8.3 Caricare il CSS in Dioxus

Due opzioni (il progetto originale usa la 1):

**1. Embed compile-time** (`main.rs`) — CSS incluso nell'eseguibile, zero file esterni a runtime:

```rust
const TAILWIND_CSS: &str = include_str!("../assets/tailwind.css");
const MAIN_CSS: &str = include_str!("../assets/main.css");

// dentro il componente root, nell'head
head { link { rel: "stylesheet", href: "{TAILWIND_CSS}" } /* ... */ }
```

Svantaggio: ogni modifica CSS richiede ricompilazione Rust (ma `dx serve` la gestisce in hot reload).

**2. Asset Dioxus** — hot reload CSS senza ricompilare:

```rust
use dioxus::prelude::*;

const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");

rsx! { head { link { rel: "stylesheet", href: TAILWIND_CSS } } }
```

### 8.4 Ordine di integrazione consigliato

```
1. Setup npm + input.css + build CSS     → verifica: tailwind.css contiene ".progress"
2. Aggiungere dipendenze Cargo           → verifica: cargo check
3. Copiare/adattare tipi (sez. 2) + test → verifica: cargo test percentage
4. Aggiungere progress_tx alla pipeline (sez. 3) → verifica: compilazione, log invii
5. Creare componente UI (sez. 4)         → verifica: dx serve, barra si muove
6. (Opzionale) wrapper modal (sez. 5)    → verifica: modal blocca chiusura durante lavoro
```

### 8.5 Varianti non-desktop (WASM/web)

In web/WASM il runtime è single-threaded: la variante parallela (`spawn_blocking` + rayon) non
esiste — usare solo il pattern sequenziale con `send(...).await`, eventualmente frazionando il
lavoro con `tokio::task::yield_now()` per non bloccare il render. Il resto del sistema (tipi,
canale, componente) è identico.
