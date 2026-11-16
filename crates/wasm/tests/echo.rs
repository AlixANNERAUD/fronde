//! Runs the `echo` test component (components/echo), built into components/dist by
//! `mise run components`.

use fronde_hub::tools::{Registry, register_meta_tools};
use fronde_wasm::{
    Allowed, ComponentHost, ComponentSpec, Credential, Limits, LoadedComponent, Origin,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn echo_wasm() -> PathBuf {
    let p = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../components/dist/echo.wasm"
    ));
    assert!(
        p.exists(),
        "{} is missing: run `mise run components`",
        p.display()
    );
    p
}

fn spec() -> ComponentSpec {
    ComponentSpec {
        name: "echo".into(),
        path: echo_wasm(),
        allow: vec![],
        credentials: vec![],
        settings: BTreeMap::new(),
        sha256: None,
        tool_prefixes: vec!["echo_".into()],
    }
}

async fn load(spec: ComponentSpec) -> LoadedComponent {
    let host = ComponentHost::new().unwrap();
    LoadedComponent::load(&host, spec).await.unwrap()
}

fn origin(server: &MockServer) -> Origin {
    server.uri().parse().unwrap()
}

/// The test servers listen on 127.0.0.1, a local address: their origins are allowed as local.
fn local(server: &MockServer) -> Allowed {
    Allowed {
        origin: origin(server),
        local: true,
    }
}

/// Answers 200 with the Authorization header it received in `x-echo-authorization`.
struct EchoAuthorization;

impl Respond for EchoAuthorization {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut response = ResponseTemplate::new(200).set_body_string("hello");
        if let Some(v) = request.headers.get("authorization") {
            response = response.insert_header("x-echo-authorization", v.to_str().unwrap());
        }
        response
    }
}

#[tokio::test]
async fn lists_and_calls_tools() {
    let echo = load(spec()).await;
    let names: Vec<&str> = echo.list_tools().iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "echo_text",
            "echo_setting",
            "echo_fetch",
            "echo_spin",
            "echo_alloc",
            "echo_sleep"
        ]
    );
    let text = &echo.list_tools()[0];
    assert_eq!(text.source, "echo");
    assert!(!text.deferred);
    assert_eq!(text.input_schema["properties"]["text"]["type"], "string");

    let out = echo.call("echo_text", &json!({"text": "salut"})).await;
    assert!(!out.is_error, "{}", out.to_text());
    assert_eq!(out.value, json!("salut"));

    let bad = echo.call("echo_text", &json!({"txt": 1})).await;
    assert!(bad.is_error);
    assert!(
        bad.to_text().contains("invalid arguments"),
        "{}",
        bad.to_text()
    );
}

#[tokio::test]
async fn settings_reach_the_guest() {
    let mut spec = spec();
    spec.settings.insert("greeting".into(), "bonjour".into());
    let echo = load(spec).await;
    let out = echo.call("echo_setting", &json!({"key": "greeting"})).await;
    assert_eq!(out.value, json!("bonjour"), "{}", out.to_text());
    let out = echo.call("echo_setting", &json!({"key": "other"})).await;
    assert_eq!(out.value, Value::Null);
}

#[tokio::test]
async fn allowed_origin_gets_host_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .and(header("authorization", "Basic bWU6aHVudGVyMg=="))
        .respond_with(EchoAuthorization)
        .expect(1)
        .mount(&server)
        .await;
    let mut spec = spec();
    spec.allow = vec![local(&server)];
    spec.credentials = vec![(
        origin(&server),
        Credential::Basic {
            username: "me".into(),
            password: "hunter2".into(),
        },
    )];
    let echo = load(spec).await;
    let out = echo
        .call(
            "echo_fetch",
            &json!({"url": format!("{}/page", server.uri())}),
        )
        .await;
    assert!(!out.is_error, "{}", out.to_text());
    assert_eq!(out.value["status"], 200);
    assert_eq!(out.value["body"], "hello");
    assert_eq!(out.value["authorization"], "Basic bWU6aHVudGVyMg==");
}

#[tokio::test]
async fn each_credential_reaches_only_its_origin() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    for (server, expected) in [(&a, "Bearer token-a"), (&b, "Bearer token-b")] {
        Mock::given(method("GET"))
            .and(header("authorization", expected))
            .respond_with(EchoAuthorization)
            .expect(1)
            .mount(server)
            .await;
    }
    let mut spec = spec();
    spec.allow = vec![local(&a), local(&b)];
    spec.credentials = vec![
        (origin(&a), Credential::Bearer("token-a".into())),
        (origin(&b), Credential::Bearer("token-b".into())),
    ];
    let echo = load(spec).await;
    for (server, expected) in [(&a, "Bearer token-a"), (&b, "Bearer token-b")] {
        let out = echo.call("echo_fetch", &json!({"url": server.uri()})).await;
        assert_eq!(out.value["authorization"], expected, "{}", out.to_text());
    }
}

#[tokio::test]
async fn waiting_on_http_is_not_charged() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;
    // 300 ms of running time, but a reply that takes 2 s: only the running time counts.
    let host = ComponentHost::with_limits(Limits {
        cpu_ticks: 3,
        ..Limits::default()
    })
    .unwrap();
    let mut spec = spec();
    spec.allow = vec![local(&server)];
    let echo = LoadedComponent::load(&host, spec).await.unwrap();
    let out = echo.call("echo_fetch", &json!({"url": server.uri()})).await;
    assert!(!out.is_error, "{}", out.to_text());
    assert_eq!(out.value["status"], 200);
}

/// Answers every request with headers at once, then one byte of the body every 100 ms.
async fn dripping_server() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let head = b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n";
                if socket.write_all(head).await.is_err() {
                    return;
                }
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if socket.write_all(b"x").await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn slow_responses_time_out() {
    let host = ComponentHost::with_limits(Limits {
        request_timeout: Duration::from_secs(1),
        ..Limits::default()
    })
    .unwrap();
    let late = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
        .mount(&late)
        .await;
    let dripping = dripping_server().await;
    let mut spec = spec();
    spec.allow = vec![
        local(&late),
        Allowed {
            origin: dripping.parse().unwrap(),
            local: true,
        },
    ];
    let echo = LoadedComponent::load(&host, spec).await.unwrap();
    for url in [late.uri(), dripping] {
        let started = Instant::now();
        let out = echo.call("echo_fetch", &json!({ "url": url })).await;
        assert!(started.elapsed() < Duration::from_secs(5), "{url}");
        assert!(out.is_error, "{url}: {}", out.to_text());
        assert!(
            out.to_text().to_lowercase().contains("timeout"),
            "{url}: {}",
            out.to_text()
        );
    }
}

#[tokio::test]
async fn memory_cap_becomes_a_tool_error() {
    let echo = load(spec()).await;
    let out = echo.call("echo_alloc", &json!({"mib": 8})).await;
    assert_eq!(out.value, json!(8 << 20), "{}", out.to_text());
    let out = echo.call("echo_alloc", &json!({"mib": 100})).await;
    assert!(out.is_error);
    assert!(
        out.to_text().contains("echo component failed"),
        "{}",
        out.to_text()
    );
    let out = echo.call("echo_text", &json!({"text": "still here"})).await;
    assert_eq!(out.value, json!("still here"));
}

#[tokio::test]
async fn denies_unlisted_origin() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let mut spec = spec();
    // Same host, another port: only the exact origin is allowed.
    spec.allow = vec![Allowed {
        origin: "http://127.0.0.1:9".parse().unwrap(),
        local: true,
    }];
    let echo = load(spec).await;
    let out = echo
        .call(
            "echo_fetch",
            &json!({"url": format!("{}/page", server.uri())}),
        )
        .await;
    assert!(out.is_error);
    assert!(
        out.to_text().to_lowercase().contains("denied"),
        "{}",
        out.to_text()
    );
}

#[tokio::test]
async fn local_addresses_need_a_local_origin() {
    let (marked, other) = (MockServer::start().await, MockServer::start().await);
    for server in [&marked, &other] {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("local"))
            .mount(server)
            .await;
    }
    // Allowed names that resolve to a loopback address; only the first is declared local.
    let url = |server: &MockServer| format!("http://localhost:{}", server.address().port());
    let mut spec = spec();
    spec.allow = vec![
        Allowed {
            origin: url(&marked).parse().unwrap(),
            local: true,
        },
        Allowed {
            origin: url(&other).parse().unwrap(),
            local: false,
        },
    ];
    let echo = load(spec).await;

    let out = echo
        .call("echo_fetch", &json!({ "url": url(&other) }))
        .await;
    assert!(out.is_error, "{}", out.to_text());
    assert!(
        out.to_text().to_lowercase().contains("prohibited"),
        "{}",
        out.to_text()
    );
    assert!(other.received_requests().await.unwrap().is_empty());

    let out = echo
        .call("echo_fetch", &json!({ "url": url(&marked) }))
        .await;
    assert!(!out.is_error, "{}", out.to_text());
    assert_eq!(out.value["body"], "local");
}

#[tokio::test]
async fn guest_cannot_set_authorization() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(EchoAuthorization)
        .mount(&server)
        .await;
    let mut spec = spec();
    spec.allow = vec![local(&server)];
    let echo = load(spec).await;
    let out = echo
        .call(
            "echo_fetch",
            &json!({"url": server.uri(), "authorization": "Bearer from-guest"}),
        )
        .await;
    assert!(out.is_error, "{}", out.to_text());
    assert!(out.to_text().contains("authorization"), "{}", out.to_text());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn infinite_loop_is_interrupted() {
    let host = ComponentHost::with_limits(Limits {
        cpu_ticks: 5,
        ..Limits::default()
    })
    .unwrap();
    let echo = LoadedComponent::load(&host, spec()).await.unwrap();
    let started = Instant::now();
    let out = echo.call("echo_spin", &json!({})).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(out.is_error);
    assert!(
        out.to_text().contains("compute budget"),
        "{}",
        out.to_text()
    );
    // Each call has its own store: the component still answers.
    let out = echo.call("echo_text", &json!({"text": "still here"})).await;
    assert_eq!(out.value, json!("still here"));
}

#[tokio::test]
async fn slow_call_times_out() {
    let host = ComponentHost::with_limits(Limits {
        call_timeout: Duration::from_millis(300),
        ..Limits::default()
    })
    .unwrap();
    let echo = LoadedComponent::load(&host, spec()).await.unwrap();
    let started = Instant::now();
    let out = echo.call("echo_sleep", &json!({"ms": 20_000})).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(out.is_error);
    assert!(out.to_text().contains("timed out"), "{}", out.to_text());
}

#[tokio::test]
async fn rejects_filesystem_imports() {
    let host = ComponentHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for interface in ["wasi:filesystem/preopens@0.2.0", "wasi:sockets/tcp@0.2.0"] {
        let path = dir.path().join("bad.wat");
        std::fs::write(
            &path,
            format!("(component (import \"{interface}\" (instance)))"),
        )
        .unwrap();
        let spec = ComponentSpec { path, ..spec() };
        let e = LoadedComponent::load(&host, spec).await.err().unwrap();
        assert!(e.to_string().contains(interface), "{e:#}");
    }
}

#[tokio::test]
async fn rejects_component_without_tools() {
    let host = ComponentHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.wat");
    std::fs::write(&path, "(component)").unwrap();
    let e = LoadedComponent::load(&host, ComponentSpec { path, ..spec() })
        .await
        .err()
        .unwrap();
    assert!(
        e.to_string().contains("fronde:component/tool world"),
        "{e:#}"
    );
}

#[tokio::test]
async fn rejects_unsupported_api_version() {
    let host = ComponentHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (wat, named) in [
        (
            r#"(component (instance $i) (export "fronde:component/tools@0.2.0" (instance $i)))"#,
            "built for fronde:component@0.2.0",
        ),
        (
            r#"(component
             (import "fronde:component/settings@1.0.0" (instance))
             (instance $i) (export "fronde:component/tools@0.1.0" (instance $i)))"#,
            "built for fronde:component@1.0.0",
        ),
        (
            r#"(component
             (import "fronde:component/settings" (instance))
             (instance $i) (export "fronde:component/tools@0.1.0" (instance $i)))"#,
            "without a version",
        ),
    ] {
        let path = dir.path().join("future.wat");
        std::fs::write(&path, wat).unwrap();
        let e = LoadedComponent::load(&host, ComponentSpec { path, ..spec() })
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("this Fronde serves 0.1"), "{e}");
        assert!(e.contains(named), "{e}");
    }
}

#[tokio::test]
async fn tools_register_in_the_hub_registry() {
    let registry = Arc::new(Registry::new());
    register_meta_tools(&registry).unwrap();
    load(spec()).await.register(&registry).unwrap();

    let visible: Vec<String> = registry.visible().into_iter().map(|d| d.name).collect();
    assert!(visible.contains(&"echo_text".to_string()), "{visible:?}");
    assert!(!visible.contains(&"echo_fetch".to_string()), "{visible:?}");
    assert_eq!(registry.get("echo_fetch").unwrap().source, "echo");

    let found = registry
        .call("search_tools", json!({"need": "fetch a web page"}))
        .await;
    assert!(
        found.to_text().contains("echo_fetch"),
        "{}",
        found.to_text()
    );

    let out = registry
        .call(
            "use_tool",
            json!({"name": "echo_text", "arguments": {"text": "via hub"}}),
        )
        .await;
    assert_eq!(out.value, json!("via hub"), "{}", out.to_text());
}
