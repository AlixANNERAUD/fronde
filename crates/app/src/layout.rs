//! The frames around the pages: the chat's header, and the centered card of the sign-in page.

use leptos::prelude::*;
use leptos_meta::Title;

use crate::components::{
    icons::{Icon, IconKind},
    ui::{ButtonSize, ButtonVariant, button_class},
};

/// The signed-in frame: a sticky header with the name and Sign out, the page below.
#[component]
pub fn Layout(children: Children) -> impl IntoView {
    view! {
        <Title text="Jarvis" />
        <header class="bg-background/95 supports-[backdrop-filter]:bg-background/80 sticky top-0 z-20 border-b backdrop-blur">
            <div class="mx-auto flex h-12 max-w-3xl items-center gap-3 px-4">
                <span class="font-semibold">"Jarvis"</span>
                <form method="post" action="/sign-out" class="ml-auto">
                    <button
                        type="submit"
                        class=button_class(ButtonVariant::Ghost, ButtonSize::Small)
                    >
                        <Icon kind=IconKind::SignOut />
                        "Se déconnecter"
                    </button>
                </form>
            </div>
        </header>
        <main class="mx-auto w-full max-w-3xl">{children()}</main>
    }
}

/// The frame of the sign-in page: the name and one centered card.
#[component]
pub fn Bare(#[prop(into)] title: String, children: Children) -> impl IntoView {
    view! {
        <Title text=format!("{title} · Jarvis") />
        <main class="mx-auto grid min-h-dvh max-w-sm content-center gap-6 p-4">
            <p class="text-center text-2xl font-semibold">"Jarvis"</p>
            <section class="bg-card text-card-foreground grid gap-4 rounded-xl border p-6 shadow-sm">
                <h1 class="leading-none font-semibold">{title}</h1>
                {children()}
            </section>
        </main>
    }
}
