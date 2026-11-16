//! The web interface of Fronde: a single-page app built by dx and served by the Fronde server.

mod app;
mod components;
mod pages;
mod socket;

fn main() {
    dioxus::launch(app::App);
}
