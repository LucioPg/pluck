use dioxus::prelude::*;

#[component]
pub fn App() -> Element {
    const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");
    rsx! {
        head { link { rel: "stylesheet", href: TAILWIND_CSS } }
        div { class: "p-8", h1 { class: "text-2xl font-bold", "ldwhelper" } }
    }
}
