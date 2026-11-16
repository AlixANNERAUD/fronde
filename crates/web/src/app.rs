//! The app: the stylesheet and the router.

use dioxus::prelude::*;

use crate::pages::{chat::ChatPage, sign_in::SignInPage};

// Linked from index.html at build time so the first paint is styled; the component below keeps
// the asset in the build.
const TAILWIND: Asset = asset!(
    "/assets/tailwind.css",
    AssetOptions::css().with_static_head(true)
);

#[derive(Clone, Debug, PartialEq, Routable)]
pub enum Route {
    #[route("/")]
    ChatPage {},
    #[route("/sign-in?:error")]
    SignInPage { error: String },
}

#[component]
pub fn App() -> Element {
    rsx! {
        document::Stylesheet { href: TAILWIND }
        Router::<Route> {}
    }
}
