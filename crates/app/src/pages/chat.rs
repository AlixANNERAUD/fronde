//! `/`: the chat. The server only renders it with a session; the island does the rest.

use leptos::prelude::*;

use crate::{islands::chat::ChatPanel, layout::Layout};

#[component]
pub fn ChatPage() -> impl IntoView {
    view! {
        <Layout>
            <ChatPanel />
        </Layout>
    }
}
