//! `/sign-in`: the name and password form. A plain form, posted to the server, which sends back
//! here with `?error=1` (wrong name or password, never told apart) or `?error=2` (too many
//! attempts).

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
        _ => Some("Prénom ou mot de passe incorrect."),
    }
}

#[component]
pub fn SignInPage(error: String) -> Element {
    rsx! {
        document::Title { "Connexion · Fronde" }
        main { class: "mx-auto flex min-h-dvh w-full max-w-sm flex-col justify-center gap-6 p-4",
            p { class: "text-center text-2xl font-semibold", "Fronde" }
            Card {
                CardHeader {
                    CardTitle { "Connexion" }
                }
                CardContent {
                    form { method: "post", action: "/auth/sign-in", class: "grid gap-4",
                        if let Some(message) = refusal(&error) {
                            Alert { variant: AlertVariant::Destructive, "{message}" }
                        }
                        div { class: "grid gap-2",
                            label {
                                r#for: "name",
                                class: "text-sm leading-none font-medium select-none",
                                "Prénom"
                            }
                            // Not required: alone on a Fronde, its owner may leave it empty.
                            // Names are compared exactly: no capital the phone would add.
                            Input {
                                id: "name",
                                name: "name",
                                autocomplete: "username",
                                autocapitalize: "none",
                                spellcheck: "false",
                                autofocus: true,
                            }
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
        assert!(page.contains(r#"action="/auth/sign-in""#), "{page}");
        assert!(page.contains(r#"name="password""#), "{page}");
        assert!(page.contains(r#"name="name""#), "{page}");
        assert!(page.contains("Prénom"), "{page}");
        assert!(page.contains(r#"autocomplete="username""#), "{page}");
        assert!(page.contains(r#"autocapitalize="none""#), "{page}");
        // The name comes first, and takes the focus.
        assert!(page.find(r#"name="name""#) < page.find(r#"name="password""#));
        assert_eq!(page.matches("autofocus=true").count(), 1, "{page}");
        assert!(
            page.contains(r#"autocomplete="current-password""#),
            "{page}"
        );
        assert!(page.contains("autofocus=true"), "{page}");
        assert!(page.contains("Se connecter"), "{page}");
        assert!(!page.contains(r#"role="alert""#), "{page}");

        let page = render("1");
        assert!(page.contains(r#"role="alert""#), "{page}");
        assert!(page.contains("Prénom ou mot de passe incorrect."), "{page}");

        let page = render("2");
        // The apostrophe is escaped.
        assert!(page.contains("Trop d"), "{page}");
        assert!(page.contains("essais, réessaie dans une minute."), "{page}");
        assert!(!page.contains("mot de passe incorrect."), "{page}");
    }
}
