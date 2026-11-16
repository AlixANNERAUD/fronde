//! Test helpers: an in-memory CalDAV server and a runner for futures that never wait.

use crate::caldav::{Request, Response, Transport};
use std::cell::RefCell;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

pub fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Run a future to completion; the fake server answers at once, so it never has to wait.
pub fn block_on<F: Future>(f: F) -> F::Output {
    match pin!(f).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("the fake server never makes a request wait"),
    }
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

type Answer = Result<Response, String>;

/// Answers each method with what was set for it (404 otherwise) and records every request.
#[derive(Clone, Default)]
pub struct FakeServer(Rc<RefCell<State>>);

#[derive(Default)]
struct State {
    answers: Vec<(&'static str, Answer)>,
    requests: Vec<Request>,
}

impl FakeServer {
    pub fn respond(&self, method: &'static str, status: u16, body: impl Into<String>) {
        let body = body.into();
        self.0
            .borrow_mut()
            .answers
            .push((method, Ok(Response { status, body })));
    }

    /// Requests with this method get no response.
    pub fn fail(&self, method: &'static str, error: &str) {
        self.0
            .borrow_mut()
            .answers
            .push((method, Err(error.to_string())));
    }

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
    async fn send(&self, req: Request) -> anyhow::Result<Response> {
        let mut state = self.0.borrow_mut();
        let answer = state
            .answers
            .iter()
            .find(|(m, _)| *m == req.method)
            .map(|(_, a)| a.clone());
        state.requests.push(req);
        match answer {
            Some(Ok(resp)) => Ok(resp),
            Some(Err(e)) => Err(anyhow::anyhow!(e)),
            None => Ok(Response {
                status: 404,
                body: String::new(),
            }),
        }
    }
}
