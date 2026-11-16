//! The web chat of jarvis, rendered on the server. The sign-in page is a plain form; the chat
//! page holds one island, the chat itself, which talks to the server over a WebSocket.
//!
//! The crate is compiled twice. With `ssr`, for the server: pages render to HTML. With
//! `hydrate`, for the browser bundle built by the `frontend` crate: only the island runs. The two
//! features are never enabled in the same build.

pub mod chat_model;
pub mod components;
pub mod islands;
pub mod layout;
pub mod pages;

use leptos::prelude::*;
use leptos_meta::{MetaTags, provide_meta_context};
use leptos_router::{
    components::{Route, Router, Routes},
    path,
};
use pages::{chat::ChatPage, sign_in::SignInPage};

/// The HTML document around every page.
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="fr">
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover" />
                <meta name="color-scheme" content="light dark" />
                <link rel="stylesheet" id="leptos" href="/pkg/jarvis.css" />
                <AutoReload options=options.clone() />
                <HydrationScripts options islands=true />
                <MetaTags />
            </head>
            <body class="bg-background text-foreground min-h-dvh font-sans antialiased">
                <App />
            </body>
        </html>
    }
}

/// The router: every page of the chat.
#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();
    view! {
        <Router>
            <Routes fallback=|| "Page introuvable.">
                <Route path=path!("/") view=ChatPage />
                <Route path=path!("/sign-in") view=SignInPage />
            </Routes>
        </Router>
    }
}
