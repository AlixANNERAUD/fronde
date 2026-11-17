//! Sends a request the gate allowed. The host name is resolved here and the connection goes to an
//! address checked after that lookup, so a second lookup cannot swap in another address. Local
//! addresses (the host, its private networks, cloud metadata) are refused unless the origin is
//! allowed as local. TLS trusts the webpki roots, as wasmtime's own sender does.

use fronde_hub::origin::{Origin, is_local, is_never_allowed};
use http::{Request, Response, Uri};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::{Arc, LazyLock};
use std::task::{Poll, ready};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use wasmtime_wasi_http::{Error, WasiBody};

trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

type Sent = (
    Response<WasiBody>,
    Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>,
);

/// Connect to `origin`, the request's as the gate parsed it, and send the request. Returns the response and the connection's
/// remaining work, which ends with the response body. `connect_timeout` covers the lookup, the
/// connection, TLS and the HTTP handshake together.
pub(crate) async fn send(
    mut request: Request<WasiBody>,
    origin: &Origin,
    connect_timeout: Duration,
    local: bool,
    component: &str,
) -> Result<Sent, Error> {
    let deadline = Instant::now() + connect_timeout;
    // The host the gate matched, not the request's raw spelling of it, is the one resolved, so
    // that the address checked is the address of the origin allowed. Only the origin is logged:
    // never a path or query, which may carry secrets.
    let host = origin.bare_host();
    let port = origin.port();

    let addresses: Vec<SocketAddr> =
        timeout_at(deadline, tokio::net::lookup_host((host.as_str(), port)))
            .await
            .map_err(|_| Error::DnsTimeout)?
            .map_err(|_| Error::DestinationNotFound)?
            .collect();
    if addresses.is_empty() {
        return Err(Error::DestinationNotFound);
    }
    // One local address is enough to refuse: a name that also resolves elsewhere is no safer.
    if addresses.iter().any(|a| is_never_allowed(a.ip())) {
        tracing::warn!(
            "{component}: {origin} resolves to a link-local or metadata address, which is \
             refused even for an origin allowed as local"
        );
        return Err(Error::DestinationIpProhibited);
    }
    if !local && addresses.iter().any(|a| is_local(a.ip())) {
        tracing::warn!(
            "{component}: {origin} resolves to a local address; allow it as \
             {{ origin = \"{origin}\", local = true }}"
        );
        return Err(Error::DestinationIpProhibited);
    }
    let tcp = timeout_at(deadline, TcpStream::connect(&addresses[..]))
        .await
        .map_err(|_| Error::ConnectionTimeout)?
        .map_err(Error::Connect)?;
    let stream: Box<dyn Stream> = if origin.is_https() {
        let name = ServerName::try_from(host).map_err(|_| Error::HttpRequestUriInvalid)?;
        let tls = timeout_at(deadline, tls()?.connect(name, tcp))
            .await
            .map_err(|_| Error::ConnectionTimeout)?
            .map_err(Error::Tls)?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let (mut sender, connection) = timeout_at(
        deadline,
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await
    .map_err(|_| Error::ConnectionTimeout)??;

    // The request line carries only the path: the scheme and authority are for proxies.
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", |p| p.as_str())
        .to_string();
    *request.uri_mut() = Uri::builder()
        .path_and_query(path)
        .build()
        .map_err(|_| Error::HttpRequestUriInvalid)?;

    // Drive the connection while waiting for the response; what remains of it is returned.
    let mut response = pin!(sender.send_request(request));
    let mut connection = Some(connection);
    let response = poll_fn(|cx| {
        if let Poll::Ready(r) = response.as_mut().poll(cx) {
            return Poll::Ready(r.map_err(Error::from));
        }
        let Some(conn) = connection.as_mut() else {
            return Poll::Pending;
        };
        let done = ready!(Pin::new(conn).poll(cx));
        connection = None;
        match done {
            Ok(()) => response.as_mut().poll(cx).map_err(Error::from),
            Err(e) => Poll::Ready(Err(Error::from(e))),
        }
    })
    .await?;
    let rest = async move {
        match connection {
            Some(conn) => conn.await.map_err(Error::from),
            None => Ok(()),
        }
    };
    Ok((
        response.map(|body| body.map_err(Error::from).boxed_unsync()),
        Box::pin(rest),
    ))
}

/// Made once: the root store is the same for every request.
static TLS: LazyLock<Result<TlsConnector, String>> = LazyLock::new(|| {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        })
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
});

fn tls() -> Result<&'static TlsConnector, Error> {
    TLS.as_ref()
        .map_err(|e| Error::InternalError(Some(e.clone())))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Runs `f` and returns what it logged.
    pub(crate) fn logged(f: impl FnOnce()) -> String {
        use std::sync::Mutex;
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buffer {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer::default();
        let out = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || out.clone())
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap()
    }

    fn refusal(uri: &str, local: bool) -> (bool, String) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut refused = false;
        let log = logged(|| {
            let request = Request::builder()
                .uri(uri)
                .body(
                    http_body_util::Empty::new()
                        .map_err(|never| match never {})
                        .boxed_unsync(),
                )
                .unwrap();
            let origin = Origin::of_request(request.uri()).unwrap();
            let sent = send(request, &origin, Duration::from_secs(2), local, "calendar");
            refused = matches!(runtime.block_on(sent), Err(Error::DestinationIpProhibited));
        });
        (refused, log)
    }

    #[test]
    fn local_refusal_names_the_fix() {
        let (refused, log) = refusal("http://127.0.0.1:5232/dav?secret=1", false);
        assert!(refused);
        assert!(log.contains("WARN"), "{log}");
        assert!(
            log.contains(
                "calendar: http://127.0.0.1:5232 resolves to a local address; allow it as \
                 { origin = \"http://127.0.0.1:5232\", local = true }"
            ),
            "{log}"
        );
        assert!(!log.contains("secret"), "{log}");
    }

    #[test]
    fn link_local_is_refused_even_as_local() {
        let (refused, log) = refusal("http://169.254.169.254/latest", true);
        assert!(refused);
        assert!(
            log.contains("calendar: http://169.254.169.254 resolves"),
            "{log}"
        );
        assert!(log.contains("link-local or metadata"), "{log}");
        assert!(!log.contains("/latest"), "{log}");
    }
}
