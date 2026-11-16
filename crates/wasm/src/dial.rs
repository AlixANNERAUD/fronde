//! Sends a request the gate allowed. The host name is resolved here and the connection goes to an
//! address checked after that lookup, so a second lookup cannot swap in another address. Local
//! addresses (the host, its private networks, cloud metadata) are refused unless the origin is
//! allowed as local. TLS trusts the webpki roots, as wasmtime's own sender does.

use http::uri::Scheme;
use http::{Request, Response, Uri};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::future::poll_fn;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
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

/// Cloud metadata services outside the ranges `is_local` already covers (most sit at
/// 169.254.169.254, which is link-local).
const METADATA: [IpAddr; 3] = [
    // Azure WireServer.
    IpAddr::V4(Ipv4Addr::new(168, 63, 129, 16)),
    // Alibaba Cloud.
    IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200)),
    // AWS over IPv6.
    IpAddr::V6(Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)),
];

/// An address of the host itself or of its networks: loopback, unspecified (which reaches the
/// host), private (10/8, 172.16/12, 192.168/16, fc00::/7), shared (100.64/10), link-local (where
/// most metadata services are) and other metadata addresses. IPv4 carried in IPv6 counts as
/// itself.
pub(crate) fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || METADATA.contains(&ip)
        }
        IpAddr::V6(v6) => match embedded_ipv4(v6) {
            Some(v4) => is_local(IpAddr::V4(v4)),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unicast_link_local()
                    || v6.is_unique_local()
                    || METADATA.contains(&ip)
            }
        },
    }
}

/// The IPv4 address an IPv6 one stands for: mapped (`::ffff:a.b.c.d`), compatible
/// (`::a.b.c.d`), NAT64 (`64:ff9b::/96`) or 6to4 (`2002::/16`).
fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let o = v6.octets();
    match v6.segments() {
        [0, 0, 0, 0, 0, 0xffff | 0, _, _] | [0x64, 0xff9b, 0, 0, 0, 0, _, _] => {
            Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]))
        }
        [0x2002, ..] => Some(Ipv4Addr::new(o[2], o[3], o[4], o[5])),
        _ => None,
    }
}

trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

type Sent = (
    Response<WasiBody>,
    Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>,
);

/// Connect to the request's origin and send it. Returns the response and the connection's
/// remaining work, which ends with the response body. `connect_timeout` covers the lookup, the
/// connection, TLS and the HTTP handshake together.
pub(crate) async fn send(
    mut request: Request<WasiBody>,
    connect_timeout: Duration,
    local: bool,
) -> Result<Sent, Error> {
    let deadline = Instant::now() + connect_timeout;
    let uri = request.uri();
    let https = uri.scheme() == Some(&Scheme::HTTPS);
    let host = uri
        .host()
        .ok_or(Error::HttpRequestUriInvalid)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });

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
    if !local && addresses.iter().any(|a| is_local(a.ip())) {
        return Err(Error::DestinationIpProhibited);
    }
    let tcp = timeout_at(deadline, TcpStream::connect(&addresses[..]))
        .await
        .map_err(|_| Error::ConnectionTimeout)?
        .map_err(Error::Connect)?;
    let stream: Box<dyn Stream> = if https {
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
mod tests {
    use super::*;

    #[test]
    fn local_addresses() {
        for local in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "10.0.0.2",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.10",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.169.254",
            "169.254.170.2",
            "100.100.100.200",
            "168.63.129.16",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "fd00:ec2::254",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "::10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2002:c0a8:101::1",
        ] {
            assert!(is_local(local.parse().unwrap()), "{local}");
        }
        for remote in [
            "8.8.8.8",
            "172.32.0.1",
            "172.15.255.255",
            "100.128.0.1",
            "100.63.255.255",
            "2001:db8::1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(!is_local(remote.parse().unwrap()), "{remote}");
        }
    }
}
