use rmcp::ServiceExt;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn config(dir: &std::path::Path, token: &str) -> fronde_hub::config::Config {
    let token_file = dir.join("token");
    fronde_hub::private::write(&token_file, token).unwrap();
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n[memory]\ngit = false\n[secrets]\nmaster_key = {{ file = {:?} }}\n",
        token_file.display().to_string(),
        dir.join("data").display().to_string(),
        dir.join("master.key").display().to_string()
    );
    toml::from_str(&toml).unwrap()
}

#[tokio::test]
async fn health_without_token() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    let r = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn mcp_rejects_missing_or_wrong_token() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    let c = reqwest::Client::new();
    let url = format!("http://{addr}/mcp");
    assert_eq!(c.post(&url).send().await.unwrap().status(), 401);
    let wrong = c
        .post(&url)
        .bearer_auth("0123456789abcdef0123456789abcdeX")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
}

#[tokio::test]
async fn mcp_initialize_and_list_tools() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(TOKEN),
    );
    let client = ().serve(transport).await.unwrap();
    let names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        names.contains(&"memory_add".to_string()) && names.contains(&"search_tools".to_string()),
        "{names:?}"
    );
}

#[tokio::test]
async fn non_localhost_host_header_is_accepted_with_token() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("Host", "fronde.example")
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .bearer_auth(TOKEN)
        .body(body)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
}

#[tokio::test]
async fn short_token_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let e = fronde::start(config(dir.path(), "short"))
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("server.token must be at least 32 characters"),
        "{e:#}"
    );
}

#[test]
fn check_reports_unknown_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, "[server]\nlistn = 1\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("listn"));
}

#[test]
fn check_accepts_valid_config() {
    let dir = tempfile::tempdir().unwrap();
    fronde_hub::private::write(dir.path().join("token"), TOKEN).unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, format!("[server]\ntoken = {{ file = {:?} }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n", dir.path().join("token").display().to_string())).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("config OK"));
}

#[cfg(unix)]
#[test]
fn check_refuses_shared_secret_file_unless_allowed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, TOKEN).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, format!("[server]\ntoken = {{ file = {:?} }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n", token.display().to_string())).unwrap();
    let run = |allow: bool| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"));
        cmd.args(["check", "--config"]).arg(&path);
        if allow {
            cmd.env("FRONDE_ALLOW_SHARED_SECRET_FILES", "1");
        }
        cmd.output().unwrap()
    };
    let out = run(false);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("readable by other users; run chmod 600"),
        "{stderr}"
    );
    assert_eq!(run(true).status.code(), Some(0));
}

#[tokio::test]
async fn healthcheck_reaches_a_running_server_only() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, _h) = fronde::start(config(dir.path(), TOKEN)).await.unwrap();
    fronde::healthcheck(addr).await.unwrap();
    // A server listening on every interface is checked over loopback.
    let unspecified = std::net::SocketAddr::from(([0, 0, 0, 0], addr.port()));
    fronde::healthcheck(unspecified).await.unwrap();

    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    assert!(fronde::healthcheck(free).await.is_err());
}

#[test]
fn healthcheck_command_fails_without_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let path = dir.path().join("fronde.toml");
    std::fs::write(&path, format!("[server]\nlisten = \"{free}\"\ntoken = {{ env = \"UNSET\" }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n")).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["healthcheck", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("cannot reach http://{free}/health"))
    );
}

fn dist(file: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../components/dist"
    ))
    .join(file);
    assert!(
        p.exists(),
        "{} is missing: run `mise run components`",
        p.display()
    );
    p
}

/// A `[[sources]]` component entry for `wasm` and its manifest, approved as they are now.
fn component_entry(name: &str, wasm: &std::path::Path, manifest: &std::path::Path) -> String {
    let mut config: fronde_hub::config::ComponentConfig = toml::from_str(&format!(
        "name = {name:?}\npath = {:?}\nmanifest = {:?}\n",
        wasm.display().to_string(),
        manifest.display().to_string()
    ))
    .unwrap();
    let hashes = fronde_wasm::Hashes::of(&config).unwrap();
    config.sha256 = Some(hashes.sha256);
    config.manifest_sha256 = Some(hashes.manifest_sha256);
    format!(
        "[[sources]]\nkind = \"component\"\nname = {name:?}\npath = {:?}\nmanifest = {:?}\nsha256 = {:?}\nmanifest_sha256 = {:?}\n",
        config.path.display().to_string(),
        config.manifest_path().display().to_string(),
        config.sha256.unwrap(),
        config.manifest_sha256.unwrap()
    )
}

#[tokio::test]
async fn broken_component_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    // Approved, but not WebAssembly: it fails to compile. Its manifest is echo's, with tool
    // names of its own.
    let broken = dir.path().join("broken.wasm");
    std::fs::write(&broken, b"not a component").unwrap();
    let broken_manifest = dir.path().join("broken.toml");
    let echo_manifest = std::fs::read_to_string(dist("echo.toml")).unwrap();
    assert!(echo_manifest.contains("tool_prefixes = [\"echo_\"]"));
    std::fs::write(
        &broken_manifest,
        echo_manifest.replace(
            "tool_prefixes = [\"echo_\"]",
            "tool_prefixes = [\"broken_\"]",
        ),
    )
    .unwrap();
    let base = config(dir.path(), TOKEN);
    let token_file = dir.path().join("token");
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ntoken = {{ file = {:?} }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n[memory]\ngit = false\n[secrets]\nmaster_key = {{ file = {:?} }}\n{}{}",
        token_file.display().to_string(),
        base.data.dir.display().to_string(),
        base.master_key_file().unwrap().display().to_string(),
        component_entry("broken", &broken, &broken_manifest),
        component_entry("echo", &dist("echo.wasm"), &dist("echo.toml")),
    );
    let (addr, _h) = fronde::start(toml::from_str(&toml).unwrap()).await.unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(TOKEN),
    );
    let client = ().serve(transport).await.unwrap();
    let names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    // The working component loaded beside it, and the hub's own tools are there.
    assert!(
        names.contains(&"memory_add".to_string()) && names.contains(&"echo_text".to_string()),
        "{names:?}"
    );
}

#[test]
fn check_prints_component_hashes_and_resolves_its_secrets() {
    let dir = tempfile::tempdir().unwrap();
    fronde_hub::private::write(dir.path().join("token"), TOKEN).unwrap();
    let wasm = dir.path().join("calendar.wasm");
    std::fs::write(&wasm, b"not compiled by check").unwrap();
    std::fs::copy(dist("calendar.toml"), dir.path().join("calendar.toml")).unwrap();
    let entry = component_entry("calendar", &wasm, &dir.path().join("calendar.toml"));
    let path = dir.path().join("fronde.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\ntoken = {{ file = \"token\" }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n\
             {entry}allow = [\"https://cloud.example\"]\n[sources.settings]\n\
             events_url = \"https://cloud.example/e/\"\nreminders_url = \"https://cloud.example/r/\"\n\
             [[sources.credentials]]\norigin = \"https://cloud.example\"\n\
             basic = {{ username = \"alix\", password = {{ env = \"FRONDE_TEST_CALDAV_PASSWORD\" }} }}\n"
        ),
    )
    .unwrap();
    let run = |password: Option<&str>| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"));
        cmd.args(["check", "--config"]).arg(&path);
        cmd.env_remove("FRONDE_TEST_CALDAV_PASSWORD");
        if let Some(p) = password {
            cmd.env("FRONDE_TEST_CALDAV_PASSWORD", p);
        }
        cmd.output().unwrap()
    };
    let out = run(None);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("source calendar: sha256 = \""), "{stdout}");
    assert!(
        stdout.contains("source calendar: manifest_sha256 = \""),
        "{stdout}"
    );
    assert!(
        stderr.contains("source calendar") && stderr.contains("FRONDE_TEST_CALDAV_PASSWORD"),
        "{stderr}"
    );
    let out = run(Some("s3cret"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("source calendar: approved: pinned by the hashes in its entry"),
        "{stdout}"
    );
}

/// `fronde check` on a configuration whose `[[sources]]` are `sources`.
fn check_sources(dir: &std::path::Path, sources: &str) -> std::process::Output {
    fronde_hub::private::write(dir.join("token"), TOKEN).unwrap();
    let path = dir.join("fronde.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\ntoken = {{ file = \"token\" }}\n[data]\ndir = \"/tmp/x\"\ntimezone = \"Europe/Paris\"\n{sources}"
        ),
    )
    .unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["check", "--config"])
        .arg(&path)
        .env("FRONDE_TEST_CALDAV_PASSWORD", "s3cret")
        .output()
        .unwrap()
}

#[test]
fn check_refuses_a_password_over_plain_http_to_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let wasm = dir.path().join("calendar.wasm");
    std::fs::write(&wasm, b"not compiled by check").unwrap();
    std::fs::copy(dist("calendar.toml"), dir.path().join("calendar.toml")).unwrap();
    let entry = component_entry("calendar", &wasm, &dir.path().join("calendar.toml"));
    let out = check_sources(
        dir.path(),
        &format!(
            "{entry}allow = [{{ origin = \"http://192.168.1.10:5232\", local = true }}]\n\
             [sources.settings]\nevents_url = \"http://192.168.1.10:5232/e/\"\n\
             reminders_url = \"http://192.168.1.10:5232/r/\"\n[[sources.credentials]]\n\
             origin = \"http://192.168.1.10:5232\"\n\
             basic = {{ username = \"alix\", password = {{ env = \"FRONDE_TEST_CALDAV_PASSWORD\" }} }}\n"
        ),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("source calendar") && stderr.contains("would travel unencrypted"),
        "{stderr}"
    );
}

/// A component approved in the database, as the owner's approval will store it: `fronde check`
/// approves it as `serve` does, and creates no database where there was none.
/// A configuration with one calendar entry without hashes whose `.wasm` is not the shipped one:
/// only a stored approval can approve it. Returns the configuration's path and the data directory.
fn unshipped_calendar(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    fronde_hub::private::write(dir.join("token"), TOKEN).unwrap();
    let wasm = dir.join("calendar.wasm");
    std::fs::write(&wasm, b"not compiled by check").unwrap();
    std::fs::copy(dist("calendar.toml"), dir.join("calendar.toml")).unwrap();
    let data = dir.join("data");
    let path = dir.join("fronde.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\ntoken = {{ file = \"token\" }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
             [[sources]]\nkind = \"component\"\nname = \"calendar\"\npath = {:?}\n\
             allow = [\"https://cloud.example\"]\n[sources.settings]\n\
             events_url = \"https://cloud.example/e/\"\nreminders_url = \"https://cloud.example/r/\"\n",
            data.display().to_string(),
            wasm.display().to_string()
        ),
    )
    .unwrap();
    (path, data)
}

fn run_check(config: &std::path::Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["check", "--config"])
        .arg(config)
        .output()
        .unwrap()
}

#[tokio::test]
async fn check_reads_the_approvals_stored_in_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let (path, data) = unshipped_calendar(dir.path());
    let check = || run_check(&path);

    let out = check();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("source calendar: not approved yet"),
        "{stderr}"
    );
    assert!(!fronde_db::path(&data).exists());

    let config = fronde_hub::config::load(&path).unwrap();
    let fronde_hub::config::SourceConfig::Component(component) = &config.sources[0] else {
        panic!("not a component")
    };
    let hashes = fronde_wasm::Hashes::of(component).unwrap();
    let db = fronde_db::open(&data).await.unwrap();
    fronde_db::approvals::add(&db, "calendar", &hashes.sha256, &hashes.manifest_sha256)
        .await
        .unwrap();
    db.close().await.unwrap();

    let out = check();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("source calendar: approved: by the owner, stored in the database"),
        "{stdout}"
    );
}

/// `fronde check` reads the database without migrating it: one an older Fronde left, without the
/// approvals table, is left exactly as it was, and approves nothing, as serving would decide.
#[tokio::test]
async fn check_leaves_an_older_database_unchanged() {
    use fronde_migration::{Migrator, MigratorTrait};
    use sea_orm::ConnectionTrait;

    let dir = tempfile::tempdir().unwrap();
    let (path, data) = unshipped_calendar(dir.path());
    fronde_hub::private::create_dir_all(&data).unwrap();
    let file = fronde_db::path(&data);
    fronde_hub::private::write(&file, b"").unwrap();
    let older = sea_orm::Database::connect(format!("sqlite://{}?mode=rw", file.display()))
        .await
        .unwrap();
    older
        .execute_unprepared("PRAGMA journal_mode = WAL")
        .await
        .unwrap();
    // Every migration before the approvals table.
    let before_approvals = Migrator::migrations()
        .iter()
        .position(|m| m.name().ends_with("_approvals"))
        .unwrap();
    Migrator::up(&older, Some(before_approvals as u32))
        .await
        .unwrap();
    let applied = Migrator::get_applied_migrations(&older)
        .await
        .unwrap()
        .len();
    older.close().await.unwrap();
    let before = std::fs::read(&file).unwrap();

    let out = run_check(&path);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("source calendar: not approved yet"),
        "{stderr}"
    );

    assert!(
        std::fs::read(&file).unwrap() == before,
        "check changed {}",
        file.display()
    );
    let db = fronde_db::open_read_only(&data).await.unwrap();
    assert!(!fronde_db::approvals::table_exists(&db).await.unwrap());
    assert_eq!(
        Migrator::get_applied_migrations(&db).await.unwrap().len(),
        applied
    );
}

/// A configuration with one calendar entry, pinned to its files, whose CalDAV password is the
/// stored secret `caldav`. Returns the configuration's path and the data directory.
fn calendar_with_stored_password(
    dir: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    fronde_hub::private::write(dir.join("token"), TOKEN).unwrap();
    let wasm = dir.join("calendar.wasm");
    std::fs::write(&wasm, b"not compiled by check").unwrap();
    std::fs::copy(dist("calendar.toml"), dir.join("calendar.toml")).unwrap();
    let entry = component_entry("calendar", &wasm, &dir.join("calendar.toml"));
    let data = dir.join("data");
    let path = dir.join("fronde.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\ntoken = {{ file = \"token\" }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n\
             {entry}allow = [\"https://cloud.example\"]\n[sources.settings]\n\
             events_url = \"https://cloud.example/e/\"\nreminders_url = \"https://cloud.example/r/\"\n\
             [[sources.credentials]]\norigin = \"https://cloud.example\"\n\
             basic = {{ username = \"alix\", password = {{ stored = \"caldav\" }} }}\n",
            data.display().to_string()
        ),
    )
    .unwrap();
    (path, data)
}

/// Store `value` as `name` for `origin` in `data`'s database, sealed with the key in `key_file`,
/// as `fronde secret set` does.
async fn store_secret(
    data: &std::path::Path,
    key_file: &std::path::Path,
    name: &str,
    origin: &str,
    value: &str,
) {
    fronde_hub::private::prepare_data_dir(data).unwrap();
    let db = fronde_db::open(data).await.unwrap();
    let (key, _) = fronde_db::master_key::load_or_create(&db, key_file)
        .await
        .unwrap();
    fronde_db::secrets::set(
        &db,
        &key,
        fronde_db::secrets::SOURCES,
        name,
        origin,
        &secrecy::SecretString::from(value.to_string()),
    )
    .await
    .unwrap();
    db.close().await.unwrap();
}

/// Every file of `dir` with its content, by name.
fn snapshot(dir: &std::path::Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name(), std::fs::read(e.path()).unwrap())
        })
        .collect();
    files.sort();
    files
}

#[tokio::test]
async fn check_reports_stored_credentials() {
    const VALUE: &str = "canary-stored-caldav-5e6f7a8b";
    let dir = tempfile::tempdir().unwrap();
    let (path, data) = calendar_with_stored_password(dir.path());
    let output = |out: &std::process::Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };

    // Not stored: said so, with the command that stores it.
    let out = run_check(&path);
    let shown = output(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(
        shown.contains("source calendar")
            && shown.contains("no secret named caldav is stored")
            && shown.contains("`fronde secret set caldav --origin https://cloud.example --config"),
        "{shown}"
    );
    assert!(!fronde_db::path(&data).exists(), "check made no database");

    // Stored for another origin.
    store_secret(
        &data,
        &dir.path().join("master.key"),
        "caldav",
        "https://other.example",
        VALUE,
    )
    .await;
    let out = run_check(&path);
    let shown = output(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(
        shown.contains(
            "the stored secret caldav is for https://other.example, not https://cloud.example"
        ),
        "{shown}"
    );
    assert!(!shown.contains(VALUE), "{shown}");

    // Stored for its origin: it opens, and its value is never shown nor anything written.
    store_secret(
        &data,
        &dir.path().join("master.key"),
        "caldav",
        "https://cloud.example",
        VALUE,
    )
    .await;
    let before = snapshot(&data);
    let out = run_check(&path);
    let shown = output(&out);
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(
        shown.contains(
            "source calendar: credential for https://cloud.example: stored secret caldav opens, \
             stored for this origin"
        ),
        "{shown}"
    );
    assert!(!shown.contains(VALUE), "{shown}");
    // SQLite's read-only open of a WAL database leaves an empty `-wal` and its index `-shm`.
    let after: Vec<_> = snapshot(&data)
        .into_iter()
        .filter(|(name, bytes)| {
            !(name == "fronde.db-shm" || name == "fronde.db-wal" && bytes.is_empty())
        })
        .collect();
    assert!(after == before, "check changed the data directory");

    // A key that did not seal it.
    let key_file = dir.path().join("master.key");
    std::fs::remove_file(&key_file).unwrap();
    fronde_hub::private::write(&key_file, format!("fronde-key-v1:{}\n", "A".repeat(43))).unwrap();
    let out = run_check(&path);
    let shown = output(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(
        shown.contains("is not the one that sealed the stored secrets"),
        "{shown}"
    );
}

/// `serve` checks the master key before anything else, whether or not a component needs it.
#[tokio::test]
async fn start_refuses_a_wrong_or_missing_master_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path(), TOKEN);
    let key_file = config.master_key_file().unwrap().to_path_buf();
    store_secret(
        &config.data.dir,
        &key_file,
        "caldav",
        "https://cloud.example",
        "v",
    )
    .await;

    std::fs::remove_file(&key_file).unwrap();
    fronde_hub::private::write(&key_file, format!("fronde-key-v1:{}\n", "A".repeat(43))).unwrap();
    let e = fronde::start(config.clone()).await.unwrap_err();
    assert!(
        format!("{e:#}").contains("is not the one that sealed the stored secrets"),
        "{e:#}"
    );

    std::fs::remove_file(&key_file).unwrap();
    let e = fronde::start(config).await.unwrap_err();
    assert!(
        format!("{e:#}").contains(&format!("the master key {} is missing", key_file.display())),
        "{e:#}"
    );
    assert!(!key_file.exists(), "no new key is made");
}

/// A key an earlier version left in the data directory: `fronde check` reads it there and moves
/// nothing, `serve` moves it to the configured file; a different key in each place stops `serve`.
#[tokio::test]
async fn a_key_in_the_data_directory_is_read_by_check_and_moved_by_serve() {
    const VALUE: &str = "canary-stored-caldav-5e6f7a8b";
    let dir = tempfile::tempdir().unwrap();
    let (path, data) = calendar_with_stored_password(dir.path());
    let legacy = data.join("master.key");
    let key_file = dir.path().join("master.key");
    store_secret(&data, &legacy, "caldav", "https://cloud.example", VALUE).await;
    let line = std::fs::read(&legacy).unwrap();

    let out = run_check(&path);
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(shown.contains("stored secret caldav opens"), "{shown}");
    assert!(legacy.exists() && !key_file.exists(), "check moved the key");

    // `serve`, here with no component: the move does not depend on one.
    let mut config = config(dir.path(), TOKEN);
    config.data.dir = data.clone();
    assert_eq!(config.master_key_file().unwrap(), key_file);
    let (_addr, server) = fronde::start(config.clone()).await.unwrap();
    server.abort();
    assert!(!legacy.exists(), "the data directory still holds the key");
    assert_eq!(std::fs::read(&key_file).unwrap(), line);

    fronde_hub::private::write(&legacy, format!("fronde-key-v1:{}\n", "A".repeat(43))).unwrap();
    let e = format!("{:#}", fronde::start(config).await.unwrap_err());
    assert!(e.contains("two different master keys"), "{e}");
    assert!(legacy.exists() && key_file.exists());
}

#[test]
fn check_approves_the_shipped_calendar_without_hashes() {
    // This binary was built after `mise run components`, from the same components/dist.
    let dir = tempfile::tempdir().unwrap();
    let out = check_sources(
        dir.path(),
        &format!(
            "[[sources]]\nkind = \"component\"\nname = \"calendar\"\npath = {:?}\n\
             allow = [\"https://cloud.example\"]\n[sources.settings]\n\
             events_url = \"https://cloud.example/e/\"\nreminders_url = \"https://cloud.example/r/\"\n",
            dist("calendar.wasm").display().to_string()
        ),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("source calendar: approved: shipped with this build (calendar)"),
        "{stdout}"
    );
}

fn cache_tempdir() -> tempfile::TempDir {
    let base = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME must be set"))
        .join(".cache/ft");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("fronde-cli-test-")
        .tempdir_in(&base)
        .unwrap()
}

#[test]
fn service_install_refuses_missing_config() {
    let dir = cache_tempdir();
    let xdg = dir.path().join("xdg");
    let missing = dir.path().join("missing.toml");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
        .args(["service", "install", "--config"])
        .arg(&missing)
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist") && stderr.contains("fronde init"),
        "{stderr}"
    );
    assert!(!xdg.exists());
}

#[test]
fn service_commands_report_clear_error_without_systemd_user_instance() {
    let dir = cache_tempdir();
    let xdg = dir.path().join("xdg");
    let cfg = dir.path().join("fronde.toml");
    std::fs::write(&cfg, "").unwrap();

    for args in [
        vec![
            "service".to_string(),
            "install".to_string(),
            "--config".to_string(),
            cfg.display().to_string(),
        ],
        vec!["service".to_string(), "uninstall".to_string()],
        vec!["service".to_string(), "status".to_string()],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
            .args(&args)
            .env("XDG_CONFIG_HOME", &xdg)
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("XDG_RUNTIME_DIR")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("systemd user services are not available here"),
            "{args:?}: {stderr}"
        );
    }
    assert!(!xdg.exists());
}

/// The runtime `mise run python` fetched into the checkout.
fn python_dir() -> std::path::PathBuf {
    let dir =
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../python")).to_path_buf();
    assert!(
        dir.join("python.wasm").exists(),
        "{} has no python.wasm: run `mise run python`",
        dir.display()
    );
    dir
}

/// Fronde, with `[python]` when `python` is set, and a client connected to its `/mcp`.
async fn with_python(
    dir: &std::path::Path,
    python: bool,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let mut config = config(dir, TOKEN);
    if python {
        config.python = Some(fronde_hub::config::PythonConfig { dir: python_dir() });
    }
    let (addr, _h) = fronde::start(config).await.unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(TOKEN),
    );
    ().serve(transport).await.unwrap()
}

async fn use_tool(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    arguments: serde_json::Value,
) -> (bool, String) {
    let args = serde_json::json!({"name": name, "arguments": arguments})
        .as_object()
        .unwrap()
        .clone();
    let r = client
        .call_tool(rmcp::model::CallToolRequestParams::new("use_tool").with_arguments(args))
        .await
        .unwrap();
    let text = match &r.content[0] {
        rmcp::model::ContentBlock::Text(t) => t.text.clone(),
        other => panic!("{other:?}"),
    };
    (r.is_error == Some(true), text)
}

#[tokio::test]
async fn python_tool_is_served_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    let client = with_python(dir.path(), true).await;
    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(
        instructions.contains("code_run_python, never mental arithmetic"),
        "{instructions}"
    );
    assert!(
        instructions.contains("python: code_run_python"),
        "{instructions}"
    );
    let (failed, text) = use_tool(
        &client,
        "code_run_python",
        serde_json::json!({"code": "sum(data) / len(data)", "data": [1, 2, 6]}),
    )
    .await;
    assert!(!failed, "{text}");
    assert!(text.contains("\"value\":3.0"), "{text}");
    let (failed, text) = use_tool(
        &client,
        "code_run_python",
        serde_json::json!({"script": "1"}),
    )
    .await;
    assert!(failed, "{text}");
    assert!(text.contains("invalid arguments"), "{text}");
}

#[tokio::test]
async fn python_tool_is_absent_without_python() {
    let dir = tempfile::tempdir().unwrap();
    let client = with_python(dir.path(), false).await;
    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(!instructions.contains("code_run_python"), "{instructions}");
    let (failed, _) = use_tool(&client, "code_run_python", serde_json::json!({"code": "1"})).await;
    assert!(failed);
}

#[test]
fn check_reports_the_python_approval() {
    let dir = cache_tempdir();
    fronde_hub::private::write(dir.path().join("token"), TOKEN).unwrap();
    let path = dir.path().join("fronde.toml");
    let write = |python: &std::path::Path| {
        std::fs::write(
            &path,
            format!(
                "[server]\ntoken = {{ file = \"token\" }}\n[data]\ndir = \"data\"\ntimezone = \"Europe/Paris\"\n[python]\ndir = {:?}\n",
                python.display().to_string()
            ),
        )
        .unwrap();
    };
    let check = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_fronde"))
            .args(["check", "--config"])
            .arg(&path)
            .output()
            .unwrap()
    };
    write(&python_dir());
    let out = check();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("python: approved (CPython 3.15.2)"),
        "{stdout}"
    );
    write(&dir.path().join("nowhere"));
    let out = check();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr.contains("python") && stderr.contains("mise run python"),
        "{stderr}"
    );
}
