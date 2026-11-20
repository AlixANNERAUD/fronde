//! The app: the stylesheet and the router.

use dioxus::prelude::*;
use fronde_protocol::ThreadId;

use crate::{
    components::thread_list::Place,
    pages::{
        sign_in::SignInPage,
        threads::{ChatPage, NewThreadPage, Shell, ThreadPage, ThreadsPage},
    },
};

// Linked from index.html at build time so the first paint is styled; the component below keeps
// the asset in the build.
const TAILWIND: Asset = asset!(
    "/assets/tailwind.css",
    AssetOptions::css().with_static_head(true)
);

// The router names each variant after the page component it renders.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Debug, PartialEq, Routable)]
#[rustfmt::skip]
pub enum Route {
    // The signed-in pages share one socket.
    #[layout(Shell)]
        #[route("/")]
        ChatPage {},
        #[route("/t/:id")]
        ThreadPage { id: i64 },
        #[route("/threads")]
        ThreadsPage {},
        #[route("/new")]
        NewThreadPage {},
    #[end_layout]
    #[route("/sign-in?:error")]
    SignInPage { error: String },
}

impl Route {
    /// The thread the address names, if it names one: `/` shows the user's home, whichever
    /// thread the server says it is.
    pub fn thread(&self) -> Option<ThreadId> {
        match self {
            Self::ThreadPage { id } => Some(ThreadId(*id)),
            Self::ChatPage {}
            | Self::ThreadsPage {}
            | Self::NewThreadPage {}
            | Self::SignInPage { .. } => None,
        }
    }
}

impl From<Place> for Route {
    fn from(place: Place) -> Self {
        match place {
            Place::Home => Self::ChatPage {},
            Place::Thread(ThreadId(id)) => Self::ThreadPage { id },
            Place::List => Self::ThreadsPage {},
            Place::New => Self::NewThreadPage {},
        }
    }
}

#[component]
pub fn App() -> Element {
    rsx! {
        document::Stylesheet { href: TAILWIND }
        Router::<Route> {}
    }
}
