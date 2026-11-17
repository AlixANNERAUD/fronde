//! Credentials `{ stored = "name" }`: the secret sealed in `fronde.db` reaches the component's
//! origin through the gate, only that origin, and is read again each time the component starts.

use fronde_db::DatabaseConnection;
use fronde_hub::config::ComponentConfig;
use fronde_hub::source::{Health, Supervisor, ToolSource, Waiting};
use fronde_hub::tools::Registry;
use fronde_wasm::{Checks, ComponentHost, ComponentSource, Credential, Hashes, StoredSecrets};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn dist(file: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../components/dist")
        .join(file);
    assert!(
        p.exists(),
        "{} is missing: run `mise run components`",
        p.display()
    );
    p
}

/// The echo component in `dir`, its manifest asking for the origin of its `url` setting and a
/// bearer credential for it.
fn echo_with_credential(dir: &Path) -> PathBuf {
    let wasm = dir.join("echo.wasm");
    std::fs::copy(dist("echo.wasm"), &wasm).unwrap();
    let manifest = std::fs::read_to_string(dist("echo.toml")).unwrap();
    std::fs::write(
        dir.join("echo.toml"),
        format!(
            "{manifest}\n[settings.properties.url]\ntype = \"string\"\nformat = \"uri\"\n\
             description = \"The server.\"\n\
             [permissions]\nnetwork = [\"{{{{ url }}}}\"]\n\
             [[credentials]]\nname = \"token\"\ninjection = \"bearer\"\norigins = [\"{{{{ url }}}}\"]\n"
        ),
    )
    .unwrap();
    wasm
}

/// The entry for the echo component in `dir`, pinned to its files, whose credential for
/// `server`'s origin is `secret`.
fn entry(dir: &Path, server: &MockServer, secret: &str) -> ComponentConfig {
    let wasm = echo_with_credential(dir);
    let origin = server.uri();
    let mut config: ComponentConfig = toml::from_str(&format!(
        "name = \"echo\"\npath = {:?}\n\
         allow = [{{ origin = {origin:?}, local = true }}]\n\
         [settings]\nurl = \"{origin}/\"\n\
         [[credentials]]\norigin = {origin:?}\nbearer = {secret}\n",
        wasm.display().to_string()
    ))
    .unwrap();
    let hashes = Hashes::of(&config).unwrap();
    config.sha256 = Some(hashes.sha256);
    config.manifest_sha256 = Some(hashes.manifest_sha256);
    config
}

/// Store `value` as `name` for `origin` in the database of `data`, making the key if needed.
async fn store(db: &DatabaseConnection, data: &Path, name: &str, origin: &str, value: &str) {
    let (key, _) = fronde_db::master_key::load_or_create(db, data)
        .await
        .unwrap();
    fronde_db::secrets::set(
        db,
        &key,
        fronde_db::secrets::SOURCES,
        name,
        origin,
        &SecretString::from(value.to_string()),
    )
    .await
    .unwrap();
}

/// Answers 200 to a request carrying `Authorization: Bearer <token>`, the header echoed in
/// `x-echo-authorization` for `echo_fetch` to show, and expects `times` of them.
async fn expect_bearer(server: &MockServer, token: &str, times: u64) {
    let authorization = format!("Bearer {token}");
    Mock::given(method("GET"))
        .and(header("authorization", authorization.as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("hello")
                .insert_header("x-echo-authorization", authorization.as_str()),
        )
        .expect(times)
        .mount(server)
        .await;
}

/// The checks `serve` makes with `db`: nothing approved but by the entry's hashes.
async fn checks(db: &DatabaseConnection, data: &Path) -> Checks {
    Checks {
        timezone: "UTC".into(),
        secrets: StoredSecrets::load(db, data).await.unwrap(),
        ..Checks::default()
    }
}

fn refused(config: &ComponentConfig, checks: &Checks) -> anyhow::Error {
    fronde_wasm::prepare(config, checks).unwrap_err()
}

#[tokio::test]
async fn stored_credential_reaches_the_gate() {
    let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = MockServer::start().await;
    expect_bearer(&server, "stored-token", 1).await;
    let db = fronde_db::open(data.path()).await.unwrap();
    // The origin as Fronde stores it; the credential spells it as the mock server's URI does.
    store(&db, data.path(), "api", &server.uri(), "stored-token").await;
    let config = entry(dir.path(), &server, "{ stored = \"api\" }");

    let checks = checks(&db, data.path()).await;
    for shown in [format!("{checks:?}"), format!("{checks:#?}")] {
        assert!(
            shown.contains("Sealed") && !shown.contains("stored-token"),
            "{shown}"
        );
    }
    let prepared = fronde_wasm::prepare(&config, &checks).unwrap();
    for shown in [format!("{prepared:?}"), format!("{prepared:#?}")] {
        assert!(!shown.contains("stored-token"), "{shown}");
    }
    let [(origin, Credential::Bearer(token))] = &prepared.spec().credentials[..] else {
        panic!("{:?}", prepared.spec().credentials)
    };
    assert_eq!(origin.to_string(), server.uri());
    assert_eq!(token.expose_secret(), "stored-token");

    let source = ComponentSource::new(config, Checks::default(), ComponentHost::new().unwrap())
        .reading_secrets_from(db, data.path().to_path_buf());
    let registry = Arc::new(Registry::new());
    Supervisor::new(registry.clone())
        .start(Arc::new(source))
        .await
        .unwrap();
    let out = registry
        .call("echo_fetch", json!({"url": format!("{}/", server.uri())}))
        .await;
    assert!(!out.is_error, "{}", out.to_text());
    assert_eq!(out.value["status"], 200);
    assert_eq!(out.value["authorization"], "Bearer stored-token");
}

#[tokio::test]
async fn basic_password_can_be_stored() {
    let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = MockServer::start().await;
    let db = fronde_db::open(data.path()).await.unwrap();
    store(&db, data.path(), "caldav", &server.uri(), "hunter2").await;
    let mut config = entry(dir.path(), &server, "{ stored = \"caldav\" }");
    let manifest = config.manifest_path();
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, text.replace("\"bearer\"", "\"basic\"")).unwrap();
    config.manifest_sha256 = Some(Hashes::of(&config).unwrap().manifest_sha256);
    let credential = &mut config.credentials[0];
    credential.basic =
        Some(toml::from_str("username = \"alix\"\npassword = { stored = \"caldav\" }").unwrap());
    credential.bearer = None;

    let prepared = fronde_wasm::prepare(&config, &checks(&db, data.path()).await).unwrap();
    let [(_, Credential::Basic { username, password })] = &prepared.spec().credentials[..] else {
        panic!("{:?}", prepared.spec().credentials)
    };
    assert_eq!(username, "alix");
    assert_eq!(password.expose_secret(), "hunter2");
}

#[tokio::test]
async fn stored_secret_for_another_origin_is_refused() {
    let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = MockServer::start().await;
    let db = fronde_db::open(data.path()).await.unwrap();
    store(
        &db,
        data.path(),
        "api",
        "https://other.example",
        "for-other",
    )
    .await;
    let config = entry(dir.path(), &server, "{ stored = \"api\" }");

    let e = refused(&config, &checks(&db, data.path()).await);
    let shown = format!("{e:#}");
    let origin = server.uri();
    assert!(
        shown.contains(&format!("credential for {origin}"))
            && shown.contains(&format!(
                "the stored secret api is for https://other.example, not {origin}"
            ))
            && shown.contains(&format!(
                "`fronde secret set api --origin {origin} --config"
            )),
        "{shown}"
    );
    assert!(!shown.contains("for-other"), "{shown}");
    // `fronde secret set` fixes it while Fronde runs.
    assert!(e.downcast_ref::<Waiting>().is_some());
}

#[tokio::test]
async fn missing_stored_secret_explains() {
    let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = MockServer::start().await;
    let db = fronde_db::open(data.path()).await.unwrap();
    let config = entry(dir.path(), &server, "{ stored = \"api\" }");
    let origin = server.uri();
    let expected = format!(
        "credential for {origin}: no secret named api is stored; store it with `fronde secret \
         set api --origin {origin} --config <your fronde.toml>`, the value on standard input"
    );

    // Nothing stored at all: no key either.
    let e = refused(&config, &checks(&db, data.path()).await);
    assert_eq!(format!("{e:#}"), expected);
    assert!(e.downcast_ref::<Waiting>().is_some());

    // Other secrets stored, not this one.
    store(&db, data.path(), "other", &origin, "x").await;
    let e = refused(&config, &checks(&db, data.path()).await);
    assert_eq!(format!("{e:#}"), expected);
}

#[tokio::test]
async fn wrong_key_is_refused() {
    let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = MockServer::start().await;
    let db = fronde_db::open(data.path()).await.unwrap();
    store(&db, data.path(), "api", &server.uri(), "stored-token").await;
    // Another valid key: 32 zero bytes.
    let key_file = fronde_db::master_key::path(data.path());
    std::fs::remove_file(&key_file).unwrap();
    fronde_hub::private::write(&key_file, format!("fronde-key-v1:{}\n", "A".repeat(43))).unwrap();
    let config = entry(dir.path(), &server, "{ stored = \"api\" }");

    let e = format!(
        "{:#}",
        StoredSecrets::load(&db, data.path()).await.unwrap_err()
    );
    assert!(
        e.contains("is not the one that sealed the stored secrets") && e.contains("restore"),
        "{e}"
    );
    let source = ComponentSource::new(config, Checks::default(), ComponentHost::new().unwrap())
        .reading_secrets_from(db.clone(), data.path().to_path_buf());
    let e = format!("{:#}", source.start().await.err().unwrap());
    assert!(
        e.contains("is not the one that sealed the stored secrets"),
        "{e}"
    );

    // No key file at all while secrets are stored.
    std::fs::remove_file(&key_file).unwrap();
    let e = format!(
        "{:#}",
        StoredSecrets::load(&db, data.path()).await.unwrap_err()
    );
    assert!(e.contains("is missing") && e.contains("restore"), "{e}");
    assert!(!key_file.exists(), "no new key is made");
}

/// The secret is read again each time the component starts: one stored after a failed start
/// applies at the supervisor's next try, and one changed applies at the next start.
#[tokio::test]
async fn restart_picks_up_a_new_secret() {
    let (dir, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = MockServer::start().await;
    expect_bearer(&server, "first", 1).await;
    expect_bearer(&server, "second", 1).await;
    let db = fronde_db::open(data.path()).await.unwrap();
    let config = entry(dir.path(), &server, "{ stored = \"api\" }");
    let source = Arc::new(
        ComponentSource::new(config, Checks::default(), ComponentHost::new().unwrap())
            .reading_secrets_from(db.clone(), data.path().to_path_buf()),
    );
    let registry = Arc::new(Registry::new());
    let supervisor = Supervisor::new(registry.clone());
    let url = json!({"url": format!("{}/", server.uri())});

    // Not stored yet: the component waits for it instead of being given up.
    let e = supervisor.start(source.clone()).await.unwrap_err();
    assert!(
        format!("{e:#}").contains("no secret named api is stored"),
        "{e:#}"
    );
    assert!(matches!(supervisor.health()["echo"], Health::Down { .. }));
    store(&db, data.path(), "api", &server.uri(), "first").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !matches!(supervisor.health()["echo"], Health::Up { .. }) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            supervisor.health()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let out = registry.call("echo_fetch", url.clone()).await;
    assert_eq!(
        out.value["authorization"],
        "Bearer first",
        "{}",
        out.to_text()
    );

    // Changed while it runs: the next start sends the new value.
    store(&db, data.path(), "api", &server.uri(), "second").await;
    let session = source.start().await.unwrap();
    let (_, fetch) = session
        .tools
        .iter()
        .find(|(def, _)| def.name == "echo_fetch")
        .unwrap();
    let out = fetch.call(url).await;
    assert_eq!(
        out.value["authorization"],
        "Bearer second",
        "{}",
        out.to_text()
    );
}
