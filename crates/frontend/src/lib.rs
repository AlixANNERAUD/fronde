//! Browser entry point: hydrates the chat island of the server-rendered pages.
//!
//! This crate is built for `wasm32` by cargo-leptos, separately from the server, so the `ssr`
//! and `hydrate` features of `jarvis-app` are never enabled together.

// Unused as far as rustc can tell, but linking the app crate is what brings its islands'
// hydration functions into the bundle.
#[allow(unused_imports)]
use jarvis_app::*;

/// Called by the script Leptos adds to every page once the bundle loads.
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_islands();
}
