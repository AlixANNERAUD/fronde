//! `/sign-in`: the password form. A plain form, posted to the server, which sends back here with
//! `?error=1` (wrong password) or `?error=2` (too many attempts).

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::{
    components::ui::{Alert, ButtonSize, ButtonVariant, INPUT, LABEL, button_class},
    layout::Bare,
};

fn refusal(error: Option<&str>) -> Option<&'static str> {
    match error? {
        "2" => Some("Trop d'essais, réessaie dans une minute."),
        _ => Some("Mot de passe incorrect."),
    }
}

#[component]
pub fn SignInPage() -> impl IntoView {
    let error = use_query_map().with_untracked(|query| query.get("error"));
    let alert = refusal(error.as_deref()).map(|message| view! { <Alert>{message}</Alert> });
    view! {
        <Bare title="Connexion">
            {alert}
            <form method="post" action="/sign-in" class="grid gap-4">
                <div class="grid gap-2">
                    <label class=LABEL for="password">
                        "Mot de passe"
                    </label>
                    <input
                        id="password"
                        name="password"
                        type="password"
                        required
                        autofocus
                        autocomplete="current-password"
                        class=INPUT
                    />
                </div>
                <button
                    type="submit"
                    class=format!("{} w-full", button_class(ButtonVariant::Default, ButtonSize::Default))
                >
                    "Se connecter"
                </button>
            </form>
        </Bare>
    }
}
