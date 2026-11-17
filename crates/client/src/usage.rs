//! How the chat names a subscription's usage windows.

use fronde_protocol::usage::WindowKind;

/// A window's short name, as the chat shows it: « 5 h », « semaine ».
pub fn window_label(kind: &WindowKind) -> String {
    match kind {
        WindowKind::Session => "5 h".to_owned(),
        WindowKind::Weekly => "semaine".to_owned(),
        WindowKind::WeeklyModel(model) => format!("semaine {model}"),
        WindowKind::Extra => "usage payant".to_owned(),
        WindowKind::Other(id) => id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_have_short_names() {
        assert_eq!(window_label(&WindowKind::Session), "5 h");
        assert_eq!(window_label(&WindowKind::Weekly), "semaine");
        assert_eq!(
            window_label(&WindowKind::WeeklyModel("opus".into())),
            "semaine opus"
        );
        assert_eq!(window_label(&WindowKind::Extra), "usage payant");
        assert_eq!(window_label(&WindowKind::Other("x".into())), "x");
    }
}
