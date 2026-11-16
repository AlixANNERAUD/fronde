//! The web interface of jarvis: a single-page app built by dx and served by the jarvis server.

use dioxus::prelude::*;

// Linked from index.html at build time so the first paint is styled; the component below keeps
// the asset in the build.
const TAILWIND: Asset = asset!(
    "/assets/tailwind.css",
    AssetOptions::css().with_static_head(true)
);

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    rsx! {
        document::Stylesheet { href: TAILWIND }
        "Jarvis"
    }
}
