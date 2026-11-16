//! The component's settings: non-secret values the host chooses for it (URLs, user names, time
//! zone), declared in the component's manifest. Credentials never go through here.

/// The setting `key`, if the host set it.
#[cfg(target_arch = "wasm32")]
pub fn get(key: &str) -> Option<String> {
    crate::bindings::fronde::component::settings::get(key)
}

/// The setting `key`, if the host set it. Natively, what
/// [`testing::set_setting`](crate::testing::set_setting) put there on this thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn get(key: &str) -> Option<String> {
    native::SETTINGS.with_borrow(|s| s.get(key).cloned())
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    thread_local! {
        pub(crate) static SETTINGS: RefCell<BTreeMap<String, String>> = const {
            RefCell::new(BTreeMap::new())
        };
    }
}
