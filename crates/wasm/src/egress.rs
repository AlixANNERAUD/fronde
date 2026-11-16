//! Outgoing HTTP from components: denied unless the origin is allowed, with credentials added by
//! the host so that they never enter a component, and sent to an address checked after the name
//! is resolved (`dial`).

use crate::dial;
use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::HeaderValue;
use http::header::{AUTHORIZATION, COOKIE};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use std::fmt;
use std::net::IpAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::time::{Instant, Sleep};
use wasmtime_wasi_http::{
    DEFAULT_FORBIDDEN_HEADERS, Error, ForbiddenHeaderFilter, RequestOptions, WasiBody,
    WasiHttpHooks,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `scheme://host[:port]`, compared on its parsed parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub(crate) https: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Origin {
    fn of(uri: &http::Uri) -> Option<Self> {
        let https = match uri.scheme_str()? {
            "https" => true,
            "http" => false,
            _ => return None,
        };
        // The connection goes to the raw authority: refuse user information rather than trust
        // host() to agree with it.
        if uri.authority()?.as_str().contains('@') {
            return None;
        }
        let host = uri.host()?.to_ascii_lowercase();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        Some(Self { https, host, port })
    }

    fn is_loopback(&self) -> bool {
        self.host == "localhost"
            || self
                .host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }
}

impl FromStr for Origin {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let url = url::Url::parse(s).with_context(|| format!("origin {s:?} is not a URL"))?;
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            other => bail!("origin {s:?}: scheme {other:?} is not http or https"),
        };
        if !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("origin {s:?} must be scheme://host[:port], with no path or user");
        }
        let host = url
            .host_str()
            .with_context(|| format!("origin {s:?} has no host"))?
            .to_string();
        let port = url
            .port_or_known_default()
            .with_context(|| format!("origin {s:?} has no port"))?;
        Ok(Self { https, host, port })
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.https { "https" } else { "http" };
        write!(f, "{scheme}://{}:{}", self.host, self.port)
    }
}

/// A resolved secret the host sends to one origin.
#[derive(Clone)]
pub enum Credential {
    Basic { username: String, password: String },
    Bearer(String),
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Basic { username, .. } => write!(f, "Basic({username}, <redacted>)"),
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
        }
    }
}

impl Credential {
    fn header(&self) -> anyhow::Result<HeaderValue> {
        let text = match self {
            Self::Basic { username, password } => {
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("{username}:{password}"))
                )
            }
            Self::Bearer(token) => format!("Bearer {token}"),
        };
        let mut value =
            HeaderValue::from_str(&text).context("credential is not a valid header value")?;
        value.set_sensitive(true);
        Ok(value)
    }
}

/// An origin a component may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    pub origin: Origin,
    /// Whether it may resolve to a local address (loopback, private, link-local, cloud metadata),
    /// which is refused otherwise: only for a service on the host or its network.
    pub local: bool,
}

#[derive(Clone)]
struct Grant {
    origin: Origin,
    local: bool,
    authorization: Option<HeaderValue>,
}

/// The origins one component may reach. Shared by every call to that component.
#[derive(Clone)]
pub(crate) struct Grants(Arc<[Grant]>);

impl Grants {
    pub(crate) fn new(
        allow: &[Allowed],
        credentials: &[(Origin, Credential)],
    ) -> anyhow::Result<Self> {
        let mut grants: Vec<Grant> = allow
            .iter()
            .map(|a| Grant {
                origin: a.origin.clone(),
                local: a.local,
                authorization: None,
            })
            .collect();
        for (origin, credential) in credentials {
            let Some(grant) = grants.iter_mut().find(|g| g.origin == *origin) else {
                bail!("credential for {origin}, which is not in the allowed origins");
            };
            if !origin.https && !origin.is_loopback() {
                bail!(
                    "credential for {origin} would travel unencrypted: use https (plain http is \
                     accepted only for localhost)"
                );
            }
            grant.authorization = Some(credential.header()?);
        }
        Ok(Self(grants.into()))
    }

    fn find(&self, uri: &http::Uri) -> Option<&Grant> {
        let origin = Origin::of(uri)?;
        self.0.iter().find(|g| g.origin == origin)
    }
}

/// The store's HTTP hooks: one per call.
pub(crate) struct Egress {
    pub(crate) grants: Grants,
    /// For each request, from sending it to the end of the response body.
    pub(crate) request_timeout: Duration,
}

impl WasiHttpHooks for Egress {
    fn forbidden_header_filter(&mut self) -> ForbiddenHeaderFilter {
        Arc::new(|name| {
            *name == AUTHORIZATION || *name == COOKIE || DEFAULT_FORBIDDEN_HEADERS.contains(name)
        })
    }

    fn send_request(
        &mut self,
        mut request: http::Request<WasiBody>,
        _guest_options: Option<RequestOptions>,
        _fut: Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send>,
    ) -> Box<
        dyn Future<
                Output = wasmtime_wasi_http::Result<(
                    http::Response<WasiBody>,
                    Box<dyn Future<Output = wasmtime_wasi_http::Result<()>> + Send>,
                )>,
            > + Send,
    > {
        let Some(grant) = self.grants.find(request.uri()) else {
            // Read and discard the guest's body before refusing: dropping it would fail the
            // guest's write first, and the guest would report a reset connection, not the denial.
            return Box::new(async move {
                let mut body = request.into_body();
                while let Some(Ok(_)) = body.frame().await {}
                Err(wasmtime_wasi_http::Error::HttpRequestDenied)
            });
        };
        if let Some(value) = &grant.authorization {
            request.headers_mut().insert(AUTHORIZATION, value.clone());
        }
        // The host's timeouts replace the guest's. Redirects are not followed: a new location is
        // a new request, checked again here.
        let deadline = Instant::now() + self.request_timeout;
        let local = grant.local;
        Box::new(async move {
            let send = dial::send(request, CONNECT_TIMEOUT, local);
            let (response, io) = tokio::time::timeout_at(deadline, send)
                .await
                .map_err(|_| Error::HttpResponseTimeout)??;
            let io = async move {
                tokio::time::timeout_at(deadline, io)
                    .await
                    .map_err(|_| Error::HttpResponseTimeout)?
            };
            Ok((
                response.map(|body| {
                    Deadline {
                        body,
                        sleep: Box::pin(tokio::time::sleep_until(deadline)),
                    }
                    .boxed_unsync()
                }),
                Box::new(io) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}

/// A response body that fails once the request's deadline has passed, however slowly it drips.
struct Deadline {
    body: WasiBody,
    sleep: Pin<Box<Sleep>>,
}

impl Body for Deadline {
    type Data = bytes::Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Some(Err(Error::HttpResponseTimeout)));
        }
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(allow: &[&str]) -> Grants {
        let allow: Vec<Allowed> = allow
            .iter()
            .map(|s| Allowed {
                origin: s.parse().unwrap(),
                local: false,
            })
            .collect();
        Grants::new(&allow, &[]).unwrap()
    }

    fn allowed_origin(o: &Origin) -> Allowed {
        Allowed {
            origin: o.clone(),
            local: true,
        }
    }

    fn allowed(g: &Grants, uri: &str) -> bool {
        g.find(&uri.parse().unwrap()).is_some()
    }

    #[test]
    fn origins_must_be_bare() {
        for bad in [
            "https://h.example/path",
            "https://u:p@h.example",
            "https://h.example?q",
            "ftp://h.example",
            "h.example",
        ] {
            assert!(bad.parse::<Origin>().is_err(), "{bad}");
        }
        assert!("https://h.example/".parse::<Origin>().is_ok());
    }

    #[test]
    fn matches_scheme_host_and_port() {
        let g = grants(&["https://Cal.Example", "http://127.0.0.1:8080"]);
        assert!(allowed(&g, "https://cal.example/dav/"));
        assert!(allowed(&g, "https://CAL.example:443/dav/"));
        assert!(allowed(&g, "http://127.0.0.1:8080/x"));
        assert!(!allowed(&g, "http://cal.example/dav/"));
        assert!(!allowed(&g, "https://cal.example:8443/"));
        assert!(!allowed(&g, "http://127.0.0.1/x"));
        assert!(!allowed(&g, "https://cal.example.evil.test/"));
        assert!(!allowed(&g, "https://evil.test/"));
    }

    #[test]
    fn userinfo_does_not_fool_the_match() {
        let g = grants(&["https://cal.example"]);
        assert!(!allowed(&g, "https://cal.example@evil.test/"));
        assert!(!allowed(&g, "https://cal.example:443@evil.test/"));
        assert!(!allowed(&g, "https://x@cal.example/"));
    }

    #[test]
    fn credentials_need_an_allowed_origin() {
        let cred = Credential::Bearer("t".into());
        let e = Grants::new(&[], &[("https://a.example".parse().unwrap(), cred)])
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("not in the allowed origins"), "{e}");
    }

    #[test]
    fn credentials_need_https_unless_loopback() {
        let cred = || Credential::Bearer("t".into());
        for origin in ["http://cal.example", "http://10.0.0.2:8080"] {
            let o: Origin = origin.parse().unwrap();
            let e = Grants::new(&[allowed_origin(&o)], &[(o.clone(), cred())])
                .err()
                .unwrap()
                .to_string();
            assert!(e.contains("unencrypted"), "{e}");
        }
        for origin in [
            "https://cal.example",
            "http://localhost:8080",
            "http://127.0.0.1:9",
            "http://[::1]:9",
        ] {
            let o: Origin = origin.parse().unwrap();
            assert!(
                Grants::new(&[allowed_origin(&o)], &[(o.clone(), cred())]).is_ok(),
                "{origin}"
            );
        }
    }

    #[test]
    fn credentials_never_print() {
        let basic = Credential::Basic {
            username: "me".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{basic:?}").contains("hunter2"));
        assert!(!format!("{:?}", Credential::Bearer("tok".into())).contains("tok"));
        assert_eq!(basic.header().unwrap(), "Basic bWU6aHVudGVyMg==");
    }
}
