//! Sends a request the gate allowed. The host name is resolved here and the connection goes to an
//! address checked after that lookup, so a second lookup cannot swap in another address: loopback,
//! link-local and cloud metadata addresses are refused unless the component may reach local
//! services. TLS trusts the webpki roots, as wasmtime's own sender does.

use http::uri::Scheme;
use http::{Request, Response, Uri};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::future::poll_fn;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Poll, ready};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use wasmtime_wasi_http::{Error, WasiBody};

/// Addresses of the cloud providers' metadata services outside the link-local ranges.
const METADATA: [IpAddr; 2] = [
    // Alibaba Cloud.
    IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200)),
    // AWS over IPv6.
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)),
];

/// Loopback, unspecified (which reaches the host itself), link-local (where most metadata
/// services are, 169.254.169.254) and other metadata addresses.
pub(crate) fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_link_local() || v4.octets()[0] == 0 || METADATA.contains(&ip)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_local(IpAddr::V4(v4)),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unicast_link_local()
                    || METADATA.contains(&ip)
            }
        },
    }
}

trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

type Sent = (
    Response<WasiBody>,
    Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>,
);

/// Connect to the request's origin and send it. Returns the response and the connection's
/// remaining work, which ends with the response body.
pub(crate) async fn send(
    mut request: Request<WasiBody>,
    connect_timeout: Duration,
    allow_local: bool,
) -> Result<Sent, Error> {
    let uri = request.uri();
    let https = uri.scheme() == Some(&Scheme::HTTPS);
    let host = uri
        .host()
        .ok_or(Error::HttpRequestUriInvalid)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });

    let addresses: Vec<SocketAddr> = timeout(
        connect_timeout,
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await
    .map_err(|_| Error::DnsTimeout)?
    .map_err(|_| Error::DestinationNotFound)?
    .collect();
    if addresses.is_empty() {
        return Err(Error::DestinationNotFound);
    }
    // One local address is enough to refuse: a name that also resolves elsewhere is no safer.
    if !allow_local && addresses.iter().any(|a| is_local(a.ip())) {
        return Err(Error::DestinationIpProhibited);
    }
    let tcp = timeout(connect_timeout, TcpStream::connect(&addresses[..]))
        .await
        .map_err(|_| Error::ConnectionTimeout)?
        .map_err(Error::Connect)?;
    let stream: Box<dyn Stream> = if https {
        let name = ServerName::try_from(host).map_err(|_| Error::HttpRequestUriInvalid)?;
        let tls = timeout(connect_timeout, tls()?.connect(name, tcp))
            .await
            .map_err(|_| Error::ConnectionTimeout)?
            .map_err(Error::Tls)?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let (mut sender, connection) = timeout(
        connect_timeout,
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

fn tls() -> Result<TlsConnector, Error> {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::InternalError(Some(e.to_string())))?
        .with_root_certificates(RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.into(),
        })
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses() {
        for local in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "169.254.169.254",
            "169.254.170.2",
            "100.100.100.200",
            "::1",
            "::",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "fd00:ec2::254",
        ] {
            assert!(is_local(local.parse().unwrap()), "{local}");
        }
        for remote in [
            "8.8.8.8",
            "10.0.0.2",
            "192.168.1.10",
            "100.100.100.201",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_local(remote.parse().unwrap()), "{remote}");
        }
    }
}
