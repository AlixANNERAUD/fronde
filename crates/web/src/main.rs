//! The web interface of jarvis: a single-page app built by dx and served by the jarvis server.

mod app;
mod components;
mod pages;
mod socket;

fn main() {
    dioxus::launch(app::App);
}
