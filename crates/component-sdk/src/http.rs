//! Outgoing HTTP. Requests go out through a [`Transport`]: [`WasiHttp`] in the component, a fake
//! in tests ([`testing::FakeServer`](crate::testing::FakeServer), feature `testing`).
//!
//! The host lets a request through only to the origins allowed for the component and adds the
//! credentials itself: do not set `Authorization` or `Cookie`, which the host refuses. Redirects
//! are not followed.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// The first value of a header, whatever its case.
    pub fn header(&self, name: &str) -> Option<&str> {
        find(&self.headers, name)
    }

    /// The body as text, with invalid UTF-8 replaced.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// The first value of a header, whatever its case.
    pub fn header(&self, name: &str) -> Option<&str> {
        find(&self.headers, name)
    }

    /// The body as text, with invalid UTF-8 replaced.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn find<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// No response came back: the host denied the request, the server was unreachable, or time ran
/// out. The message says which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Sends one HTTP request. A component is single-threaded, so the future need not be `Send`.
#[allow(async_fn_in_trait)]
pub trait Transport {
    async fn send(&self, request: Request) -> Result<Response, Error>;
}

#[cfg(target_arch = "wasm32")]
pub use wasi::WasiHttp;

#[cfg(target_arch = "wasm32")]
mod wasi {
    use super::{Error, Request, Response, Transport};
    use wstd::http::{Body, BodyExt, Client, Method};

    /// WASI HTTP, through the host's allowlist; the host also sets the time limits.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct WasiHttp;

    fn failed(e: impl std::fmt::Display) -> Error {
        Error(format!("{e:#}"))
    }

    impl Transport for WasiHttp {
        async fn send(&self, request: Request) -> Result<Response, Error> {
            let mut builder = wstd::http::Request::builder()
                .method(Method::from_bytes(request.method.as_bytes()).map_err(failed)?)
                .uri(request.url.as_str());
            for (name, value) in &request.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            let body = if request.body.is_empty() {
                Body::empty()
            } else {
                Body::from(request.body)
            };
            let response = Client::new()
                .send(builder.body(body).map_err(failed)?)
                .await
                .map_err(failed)?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(n, v)| Some((n.as_str().to_string(), v.to_str().ok()?.to_string())))
                .collect();
            let body = response
                .into_body()
                .into_boxed_body()
                .collect()
                .await
                .map_err(failed)?
                .to_bytes()
                .to_vec();
            Ok(Response {
                status,
                headers,
                body,
            })
        }
    }
}
