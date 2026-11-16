//! `/sign-in`: the password form. A plain form, posted to the server, which sends back here with
//! `?error=1` (wrong password) or `?error=2` (too many attempts).

use dioxus::prelude::*;

use crate::components::ui::{
    alert::{Alert, AlertVariant},
    button::Button,
    card::{Card, CardContent, CardHeader, CardTitle},
    input::{Input, InputType},
};

/// Why the last attempt failed, from the `error` the server put in the address.
fn refusal(error: &str) -> Option<&'static str> {
    match error {
        "" => None,
        "2" => Some("Trop d'essais, réessaie dans une minute."),
        _ => Some("Mot de passe incorrect."),
    }
}

#[component]
pub fn SignInPage(error: String) -> Element {
    rsx! {
        document::Title { "Connexion · Jarvis" }
        main { class: "mx-auto flex min-h-dvh w-full max-w-sm flex-col justify-center gap-6 p-4",
            p { class: "text-center text-2xl font-semibold", "Jarvis" }
            Card {
                CardHeader {
                    CardTitle { "Connexion" }
                }
                CardContent {
                    form { method: "post", action: "/sign-in", class: "grid gap-4",
                        if let Some(message) = refusal(&error) {
                            Alert { variant: AlertVariant::Destructive, "{message}" }
                        }
                        div { class: "grid gap-2",
                            label {
                                r#for: "password",
                                class: "text-sm leading-none font-medium select-none",
                                "Mot de passe"
                            }
                            Input {
                                r#type: InputType::Password,
                                id: "password",
                                name: "password",
                                autocomplete: "current-password",
                                autofocus: true,
                                required: true,
                            }
                        }
                        Button { button_type: "submit", class: "w-full", "Se connecter" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(error: &str) -> String {
        dioxus_ssr::render_element(rsx! { SignInPage { error } })
    }

    #[test]
    fn sign_in_shows_error_texts() {
        let page = render("");
        assert!(page.contains(r#"method="post""#), "{page}");
        assert!(page.contains(r#"action="/sign-in""#), "{page}");
        assert!(page.contains(r#"name="password""#), "{page}");
        assert!(
            page.contains(r#"autocomplete="current-password""#),
            "{page}"
        );
        assert!(page.contains("autofocus=true"), "{page}");
        assert!(page.contains("Se connecter"), "{page}");
        assert!(!page.contains(r#"role="alert""#), "{page}");

        let page = render("1");
        assert!(page.contains(r#"role="alert""#), "{page}");
        assert!(page.contains("Mot de passe incorrect."), "{page}");

        let page = render("2");
        // The apostrophe is escaped.
        assert!(page.contains("Trop d"), "{page}");
        assert!(page.contains("essais, réessaie dans une minute."), "{page}");
        assert!(!page.contains("Mot de passe incorrect."), "{page}");
    }
}
