//! Fakes to test a component natively: an in-memory HTTP server, settings, and a runner for
//! futures that never wait.

use crate::http::{Error, Request, Response, Transport};
use std::cell::RefCell;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Set what [`settings::get`](crate::settings::get) returns on this thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn set_setting(key: &str, value: Option<&str>) {
    crate::settings::native::SETTINGS.with_borrow_mut(|s| match value {
        Some(v) => s.insert(key.to_string(), v.to_string()),
        None => s.remove(key),
    });
}

/// Run a future to completion. [`FakeServer`] answers at once, so the future never has to wait;
/// one that does panics.
pub fn block_on<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("the future waited, which the fakes never make it do"),
    }
}

type Answer = Result<Response, Error>;

/// Answers each method with what was set for it (404 otherwise) and records every request.
/// Clones share the same answers and records.
#[derive(Clone, Default)]
pub struct FakeServer(Rc<RefCell<State>>);

#[derive(Default)]
struct State {
    answers: Vec<(String, Answer)>,
    requests: Vec<Request>,
}

impl FakeServer {
    /// Requests with this method get this response; the first answer set for a method wins.
    pub fn respond(&self, method: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.0
            .borrow_mut()
            .answers
            .push((method.to_string(), Ok(Response::new(status, body))));
    }

    /// Requests with this method get no response, as when the host denies them.
    pub fn fail(&self, method: &str, error: &str) {
        self.0
            .borrow_mut()
            .answers
            .push((method.to_string(), Err(Error::new(error))));
    }

    /// The requests received with this method, oldest first.
    pub fn requests(&self, method: &str) -> Vec<Request> {
        self.0
            .borrow()
            .requests
            .iter()
            .filter(|r| r.method == method)
            .cloned()
            .collect()
    }
}

impl Transport for FakeServer {
    async fn send(&self, request: Request) -> Result<Response, Error> {
        let mut state = self.0.borrow_mut();
        let answer = state
            .answers
            .iter()
            .find(|(m, _)| *m == request.method)
            .map(|(_, a)| a.clone());
        state.requests.push(request);
        answer.unwrap_or_else(|| Ok(Response::new(404, Vec::new())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;

    #[test]
    fn fake_server_answers_by_method_and_records() {
        let server = FakeServer::default();
        server.respond("GET", 200, "hello");
        server.fail("PUT", "denied");
        let get = block_on(server.send(Request::new("GET", "http://a.test/x"))).unwrap();
        assert_eq!((get.status, get.text().as_str()), (200, "hello"));
        let put = Request::new("PUT", "http://a.test/x")
            .with_header("Content-Type", "text/plain")
            .with_body("body");
        assert_eq!(
            block_on(server.send(put)).unwrap_err().to_string(),
            "denied"
        );
        let missing = block_on(server.send(Request::new("DELETE", "http://a.test/x"))).unwrap();
        assert_eq!(missing.status, 404);
        let [put] = &server.requests("PUT")[..] else {
            panic!("one PUT expected");
        };
        assert_eq!(put.header("content-type"), Some("text/plain"));
        assert_eq!(put.text(), "body");
    }

    #[test]
    fn settings_come_from_the_test() {
        assert_eq!(settings::get("url"), None);
        set_setting("url", Some("https://a.test"));
        assert_eq!(settings::get("url").as_deref(), Some("https://a.test"));
        set_setting("url", None);
        assert_eq!(settings::get("url"), None);
    }
}
