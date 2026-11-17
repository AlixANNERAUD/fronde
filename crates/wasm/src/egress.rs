//! Outgoing HTTP from components: denied unless the origin is allowed, with credentials added by
//! the host so that they never enter a component, and sent to an address checked after the name
//! is resolved (`dial`).

use crate::dial;
use anyhow::{Context, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use fronde_hub::origin::Origin;
use http::HeaderValue;
use http::header::{AUTHORIZATION, COOKIE};
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use secrecy::zeroize::Zeroize;
use secrecy::{ExposeSecret, SecretString};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::time::{Instant, Sleep};
use wasmtime_wasi_http::{
    DEFAULT_FORBIDDEN_HEADERS, Error, ForbiddenHeaderFilter, RequestOptions, WasiBody,
    WasiHttpHooks,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A resolved secret the host sends to one origin.
#[derive(Debug, Clone)]
pub enum Credential {
    Basic {
        username: String,
        password: SecretString,
    },
    Bearer(SecretString),
}

impl Credential {
    /// Refuses a credential that could not travel in a header, without building one: the header
    /// is built for each request and dropped with it, so no plain copy lives as long as the
    /// component.
    fn check(&self) -> anyhow::Result<()> {
        match self {
            // Base64 output is always a valid header value.
            Self::Basic { .. } => Ok(()),
            Self::Bearer(token) => {
                ensure!(
                    token.expose_secret().bytes().all(header_byte),
                    "credential is not a valid header value"
                );
                Ok(())
            }
        }
    }

    fn header(&self) -> anyhow::Result<HeaderValue> {
        let mut text = match self {
            Self::Basic { username, password } => {
                let mut raw = format!("{username}:{}", password.expose_secret());
                let mut encoded = STANDARD.encode(&raw);
                raw.zeroize();
                let header = format!("Basic {encoded}");
                encoded.zeroize();
                header
            }
            Self::Bearer(token) => format!("Bearer {}", token.expose_secret()),
        };
        let value = HeaderValue::from_str(&text).context("credential is not a valid header value");
        text.zeroize();
        let mut value = value?;
        value.set_sensitive(true);
        Ok(value)
    }
}

/// What `HeaderValue::from_str` accepts: visible ASCII, tab, space and obs-text.
fn header_byte(b: u8) -> bool {
    b == b'\t' || (b >= 0x20 && b != 0x7f)
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
    credential: Option<Arc<Credential>>,
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
                credential: None,
            })
            .collect();
        for (origin, credential) in credentials {
            let Some(grant) = grants.iter_mut().find(|g| g.origin == *origin) else {
                bail!("credential for {origin}, which is not in the allowed origins");
            };
            if !origin.carries_credentials() {
                bail!(
                    "credential for {origin} would travel unencrypted: use https (plain http is \
                     accepted only for localhost)"
                );
            }
            credential.check()?;
            grant.credential = Some(Arc::new(credential.clone()));
        }
        Ok(Self(grants.into()))
    }

    /// The grant for the request's origin, and that origin.
    fn find(&self, uri: &http::Uri) -> Option<(&Grant, Origin)> {
        let origin = Origin::of_request(uri)?;
        let grant = self.0.iter().find(|g| g.origin == origin)?;
        Some((grant, origin))
    }
}

/// The store's HTTP hooks: one per call.
pub(crate) struct Egress {
    pub(crate) grants: Grants,
    /// The component's name, for the log.
    pub(crate) component: Arc<str>,
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
        let Some((grant, origin)) = self.grants.find(request.uri()) else {
            warn_unlisted(&self.component, request.uri());
            // Read and discard the guest's body before refusing: dropping it would fail the
            // guest's write first, and the guest would report a reset connection, not the denial.
            return Box::new(async move {
                let mut body = request.into_body();
                while let Some(Ok(_)) = body.frame().await {}
                Err(wasmtime_wasi_http::Error::HttpRequestDenied)
            });
        };
        if let Some(credential) = &grant.credential {
            // Built for this request only, and dropped with it.
            match credential.header() {
                Ok(value) => {
                    request.headers_mut().insert(AUTHORIZATION, value);
                }
                // Unreachable after `check`; refused like an unlisted origin all the same.
                Err(e) => {
                    tracing::warn!("{}: the credential for {origin}: {e:#}", self.component);
                    return Box::new(async move {
                        let mut body = request.into_body();
                        while let Some(Ok(_)) = body.frame().await {}
                        Err(Error::InternalError(Some(
                            "the host could not add the credential".into(),
                        )))
                    });
                }
            }
        }
        // The host's timeouts replace the guest's. Redirects are not followed: a new location is
        // a new request, checked again here.
        let deadline = Instant::now() + self.request_timeout;
        let local = grant.local;
        let credential = grant.credential.is_some();
        let component = self.component.clone();
        Box::new(async move {
            let send = dial::send(
                request,
                &origin,
                CONNECT_TIMEOUT,
                local,
                credential,
                &component,
            );
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

/// Tell the owner a request went to an origin the component may not reach. Only the origin is
/// logged, never the path or query.
fn warn_unlisted(component: &str, uri: &http::Uri) {
    let origin = match Origin::of_request(uri) {
        Some(origin) => origin.to_string(),
        None => uri.scheme_str().zip(uri.host()).map_or_else(
            || "an invalid URL".to_string(),
            |(scheme, host)| format!("{scheme}://{host}"),
        ),
    };
    tracing::warn!("{component}: {origin} is not in its allow list; add it to the source's allow");
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
    fn unlisted_origin_is_logged_without_path_or_query() {
        let log = crate::dial::tests::logged(|| {
            warn_unlisted(
                "calendar",
                &"https://x.example/dav?token=s3cret".parse().unwrap(),
            );
        });
        assert!(
            log.contains(
                "calendar: https://x.example is not in its allow list; add it to the \
                 source's allow"
            ),
            "{log}"
        );
        assert!(!log.contains("s3cret") && !log.contains("/dav"), "{log}");
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
        // Another spelling of an allowed address is the same origin.
        assert!(allowed(&g, "http://0x7f.1:8080/x"));
        assert!(allowed(&g, "http://2130706433:8080/x"));
        let g = grants(&["http://[::1]:9"]);
        assert!(allowed(&g, "http://[0:0:0:0:0:0:0:1]:9/x"));
        assert!(!allowed(&g, "http://[::2]:9/x"));
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
        for origin in [
            "http://cal.example",
            "http://10.0.0.2:8080",
            "http://localhost.:8080",
        ] {
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
    fn credentials_are_checked_without_building_a_header() {
        let o: Origin = "https://cal.example".parse().unwrap();
        let allow = [allowed_origin(&o)];
        let e = Grants::new(
            &allow,
            &[(o.clone(), Credential::Bearer("tok\nen-s3cret".into()))],
        )
        .err()
        .unwrap();
        let e = format!("{e:#} {e:?}");
        assert!(e.contains("not a valid header value"), "{e}");
        assert!(!e.contains("s3cret"), "{e}");
        // Any username and password make a valid Basic value: base64 is header-safe.
        let basic = Credential::Basic {
            username: "al\u{e9}x\n".into(),
            password: "p\u{0}w\r\n".into(),
        };
        let g = Grants::new(&allow, &[(o.clone(), basic)]).unwrap();
        let (grant, _) = g
            .find(&"https://cal.example/dav/".parse().unwrap())
            .unwrap();
        let header = grant.credential.as_ref().unwrap().header().unwrap();
        assert!(header.is_sensitive());
        for byte in [b'\t', b' ', b'~', 0x80, 0xff] {
            assert!(header_byte(byte), "{byte:#x}");
            assert!(HeaderValue::from_bytes(&[b'a', byte, b'b']).is_ok());
        }
        for byte in [0, b'\n', b'\r', 0x1f, 0x7f] {
            assert!(!header_byte(byte), "{byte:#x}");
            assert!(HeaderValue::from_bytes(&[b'a', byte, b'b']).is_err());
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
        let header = basic.header().unwrap();
        assert_eq!(header, "Basic bWU6aHVudGVyMg==");
        assert!(header.is_sensitive());
    }
}
