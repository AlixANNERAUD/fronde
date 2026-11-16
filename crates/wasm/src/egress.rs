//! Outgoing HTTP from components: denied unless the origin is allowed, with credentials added by
//! the host so that they never enter a component.

use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::HeaderValue;
use http::header::{AUTHORIZATION, COOKIE};
use http_body_util::BodyExt;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use wasmtime_wasi_http::{
    DEFAULT_FORBIDDEN_HEADERS, ForbiddenHeaderFilter, RequestOptions, WasiBody, WasiHttpHooks,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed for the response headers, and then between two chunks of the body.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// `scheme://host[:port]`, compared on its parsed parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    https: bool,
    host: String,
    port: u16,
}

impl Origin {
    fn of(uri: &http::Uri) -> Option<Self> {
        let https = match uri.scheme_str()? {
            "https" => true,
            "http" => false,
            _ => return None,
        };
        // host() and port_u16(), never the raw authority, which may carry user information.
        let host = uri.host()?.to_ascii_lowercase();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        Some(Self { https, host, port })
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

#[derive(Clone)]
struct Grant {
    origin: Origin,
    authorization: Option<HeaderValue>,
}

/// The origins one component may reach. Shared by every call to that component.
#[derive(Clone)]
pub(crate) struct Grants(Arc<[Grant]>);

impl Grants {
    pub(crate) fn new(
        allow: &[Origin],
        credentials: &[(Origin, Credential)],
    ) -> anyhow::Result<Self> {
        let mut grants: Vec<Grant> = allow
            .iter()
            .map(|origin| Grant {
                origin: origin.clone(),
                authorization: None,
            })
            .collect();
        for (origin, credential) in credentials {
            let Some(grant) = grants.iter_mut().find(|g| g.origin == *origin) else {
                bail!("credential for {origin}, which is not in the allowed origins");
            };
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
        let options = RequestOptions {
            connect_timeout: Some(CONNECT_TIMEOUT),
            first_byte_timeout: Some(RESPONSE_TIMEOUT),
            between_bytes_timeout: Some(RESPONSE_TIMEOUT),
        };
        Box::new(async move {
            let (response, io) =
                wasmtime_wasi_http::default_send_request(request, Some(options)).await?;
            Ok((
                response.map(BodyExt::boxed_unsync),
                Box::new(io) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(allow: &[&str]) -> Grants {
        let allow: Vec<Origin> = allow.iter().map(|s| s.parse().unwrap()).collect();
        Grants::new(&allow, &[]).unwrap()
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
