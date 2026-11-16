//! The browser APIs the chat island uses. Islands are also rendered on the server, where their
//! effects and event handlers never run, so each item has an inert server twin with the same
//! signature and the island needs no `cfg` of its own.

use std::time::Duration;

#[cfg(feature = "hydrate")]
mod imp {
    use leptos::prelude::{document, window};
    use wasm_bindgen::{JsCast, closure::Closure};

    /// An open WebSocket and its handlers. Dropping it closes the socket without calling them.
    pub struct Socket {
        socket: web_sys::WebSocket,
        _on_open: Closure<dyn FnMut()>,
        _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
        _on_close: Closure<dyn FnMut(web_sys::CloseEvent)>,
    }

    impl Socket {
        pub fn open(
            path: &str,
            on_open: impl FnMut() + 'static,
            mut on_message: impl FnMut(String) + 'static,
            mut on_close: impl FnMut(u16) + 'static,
        ) -> Option<Self> {
            let location = window().location();
            let scheme = if location.protocol().ok()? == "https:" {
                "wss"
            } else {
                "ws"
            };
            let socket =
                web_sys::WebSocket::new(&format!("{scheme}://{}{path}", location.host().ok()?))
                    .ok()?;
            let on_open = Closure::<dyn FnMut()>::new(on_open);
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(
                move |event: web_sys::MessageEvent| {
                    if let Some(text) = event.data().as_string() {
                        on_message(text);
                    }
                },
            );
            // A failed connection is closed too, so `onerror` adds nothing.
            let on_close = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(
                move |event: web_sys::CloseEvent| {
                    on_close(event.code());
                },
            );
            socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
            Some(Self {
                socket,
                _on_open: on_open,
                _on_message: on_message,
                _on_close: on_close,
            })
        }

        pub fn send(&self, text: &str) -> bool {
            self.socket.ready_state() == web_sys::WebSocket::OPEN
                && self.socket.send_with_str(text).is_ok()
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            self.socket.set_onopen(None);
            self.socket.set_onmessage(None);
            self.socket.set_onclose(None);
            let _ = self.socket.close();
        }
    }

    pub fn go_to(path: &str) {
        let _ = window().location().assign(path);
    }

    pub fn on_visible(callback: impl Fn() + 'static) {
        let listener = Closure::<dyn Fn()>::new(move || {
            if document().visibility_state() == web_sys::VisibilityState::Visible {
                callback();
            }
        });
        let _ = document().add_event_listener_with_callback(
            "visibilitychange",
            listener.as_ref().unchecked_ref(),
        );
        // Listens for as long as the page is open.
        listener.forget();
    }

    fn page_height() -> f64 {
        document()
            .document_element()
            .map_or(0.0, |root| f64::from(root.scroll_height()))
    }

    pub fn near_end() -> bool {
        let window = window();
        let viewport = window
            .inner_height()
            .ok()
            .and_then(|height| height.as_f64())
            .unwrap_or(0.0);
        let scrolled = window.scroll_y().unwrap_or(0.0);
        scrolled + viewport >= page_height() - 80.0
    }

    pub fn scroll_to_end() {
        window().scroll_to_with_x_and_y(0.0, page_height());
    }
}

/// A WebSocket to this site; see [`Socket::open`].
#[cfg(feature = "hydrate")]
pub use imp::Socket;

/// The server never opens a socket.
#[cfg(not(feature = "hydrate"))]
pub struct Socket;

#[cfg(not(feature = "hydrate"))]
impl Socket {
    /// Opens a WebSocket to `path` on this site (`wss` on https); `None` when the browser
    /// refuses. `on_close` runs with the close code when it closes or fails to connect.
    pub fn open(
        path: &str,
        on_open: impl FnMut() + 'static,
        on_message: impl FnMut(String) + 'static,
        on_close: impl FnMut(u16) + 'static,
    ) -> Option<Self> {
        let _ = (path, on_open, on_message, on_close);
        None
    }

    /// Sends a text message; false when the socket is not open.
    pub fn send(&self, text: &str) -> bool {
        let _ = text;
        false
    }
}

/// Runs `callback` once, after `delay`.
pub fn after(delay: Duration, callback: impl FnOnce() + 'static) {
    #[cfg(feature = "hydrate")]
    leptos::prelude::set_timeout(callback, delay);
    #[cfg(not(feature = "hydrate"))]
    let _ = (delay, callback);
}

/// Leaves for another page of this site.
pub fn go_to(path: &str) {
    #[cfg(feature = "hydrate")]
    imp::go_to(path);
    #[cfg(not(feature = "hydrate"))]
    let _ = path;
}

/// Runs `callback` each time the page becomes visible again.
pub fn on_visible(callback: impl Fn() + 'static) {
    #[cfg(feature = "hydrate")]
    imp::on_visible(callback);
    #[cfg(not(feature = "hydrate"))]
    let _ = callback;
}

/// Whether the page is scrolled to (or near) its end.
pub fn near_end() -> bool {
    #[cfg(feature = "hydrate")]
    {
        imp::near_end()
    }
    #[cfg(not(feature = "hydrate"))]
    {
        false
    }
}

/// Scrolls to the end of the page once what changed is drawn.
pub fn scroll_to_end_soon() {
    #[cfg(feature = "hydrate")]
    leptos::prelude::request_animation_frame(imp::scroll_to_end);
}
