use secrecy::ExposeSecret;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const CANARY: &str = "canary-cli-secret-4d5e6f7a8b";

/// A configuration in `dir`. The binary would use the platform's keychain: its key is kept in a
/// file, so that no test touches a real keychain (the keychain and systemd backends are tested
/// below with fakes).
fn config(dir: &Path) -> PathBuf {
    fronde_hub::private::write(dir.join("token"), TOKEN).unwrap();
    let path = dir.join("fronde.toml");
    std::fs::write(
        &path,
        format!(
            "[server]\ntoken = {{ file = \"token\" }}\n[data]\ndir = {:?}\ntimezone = \"Europe/Paris\"\n[secrets]\nbackend = \"file\"\n",
            dir.join("data").display().to_string()
        ),
    )
    .unwrap();
    path
}

/// [`config`] in `auto`, loaded, with `keychain` instead of the platform's and no systemd
/// credentials unless given.
fn auto_config(
    dir: &Path,
    keychain: &fronde_db::master_key::FakeKeychain,
    credentials: Option<PathBuf>,
) -> fronde_hub::config::Config {
    let path = config(dir);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("backend = \"file\"", "backend = \"auto\""),
    )
    .unwrap();
    let mut config = fronde_hub::config::load(&path).unwrap();
    config
        .secrets
        .set_keychain(keychain.keychain(&config.data.dir));
    config.secrets.set_credentials_dir(credentials);
    config
}

fn fronde(args: &[&str], config: &Path, stdin: Option<&[u8]>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fronde"))
        // Never the machine's keychain, whatever the environment the tests run in.
        .env("FRONDE_KEYCHAIN", "off")
        .args(args)
        .arg("--config")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(bytes) = stdin {
        input.write_all(bytes).unwrap();
    }
    drop(input);
    child.wait_with_output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

async fn stored(config: &Path, name: &str) -> String {
    let config = fronde_hub::config::load(config).unwrap();
    let db = fronde_db::open(&config.data.dir).await.unwrap();
    let key = fronde_db::master_key::load(&db, config.master_key_file().unwrap())
        .await
        .unwrap()
        .unwrap();
    let sealed = fronde_db::secrets::load_sealed(&db, fronde_db::secrets::SOURCES)
        .await
        .unwrap();
    let row = sealed.iter().find(|s| s.name == name).unwrap();
    row.open(&key).unwrap().expose_secret().to_owned()
}

#[tokio::test]
async fn set_list_and_remove_from_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = config(dir.path());

    let out = fronde(
        &[
            "secret",
            "set",
            "caldav",
            "--origin",
            "HTTPS://Cloud.Example.org:443",
        ],
        &path,
        Some(format!("{CANARY}\n").as_bytes()),
    );
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(shown.contains("created the master key"), "{shown}");
    assert!(
        shown.contains("stored secret caldav for https://cloud.example.org"),
        "{shown}"
    );
    // The key is beside the configuration, not in the data directory.
    let key_file = dir.path().join("master.key");
    assert!(shown.contains(&key_file.display().to_string()), "{shown}");
    assert!(key_file.is_file());
    assert!(!dir.path().join("data/master.key").exists());
    assert!(!shown.contains(CANARY), "{shown}");
    // The final line break is not part of the value; the origin is stored canonical.
    assert_eq!(stored(&path, "caldav").await, CANARY);

    let out = fronde(
        &[
            "secret",
            "set",
            "caldav",
            "--origin",
            "https://cloud.example.org",
        ],
        &path,
        Some(b"replaced\r\n"),
    );
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(!shown.contains("created the master key"), "{shown}");
    assert_eq!(stored(&path, "caldav").await, "replaced");

    let out = fronde(&["secret", "list"], &path, None);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(
        shown.contains("caldav\thttps://cloud.example.org\t"),
        "{shown}"
    );
    assert!(
        !shown.contains("replaced") && !shown.contains(CANARY),
        "{shown}"
    );

    let out = fronde(&["secret", "remove", "caldav"], &path, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let out = fronde(&["secret", "remove", "caldav"], &path, None);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(
        shown.contains("no stored secret is named caldav"),
        "{shown}"
    );
    let out = fronde(&["secret", "list"], &path, None);
    assert!(text(&out).contains("no stored secret"), "{}", text(&out));
}

/// The value is never an argument: one given there is refused, without being shown, and nothing
/// is stored.
#[test]
fn set_takes_no_value_as_an_argument() {
    let dir = tempfile::tempdir().unwrap();
    let path = config(dir.path());
    let hyphenated = format!("-{CANARY}");
    let flag = format!("--value={CANARY}");
    for given in [
        vec!["caldav", CANARY, "--origin", "https://cloud.example"],
        vec!["caldav", "--origin", "https://cloud.example", CANARY],
        vec!["caldav", &hyphenated, "--origin", "https://cloud.example"],
        vec!["caldav", "--origin", "https://cloud.example", &flag],
    ] {
        let args: Vec<&str> = ["secret", "set"].into_iter().chain(given).collect();
        let out = fronde(&args, &path, Some(b"from stdin"));
        let shown = text(&out);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {shown}");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains(CANARY)
                && !String::from_utf8_lossy(&out.stderr).contains(CANARY),
            "{args:?}: the argument was shown: {shown}"
        );
        assert!(shown.contains("standard input"), "{args:?}: {shown}");
    }
    let out = fronde(&["secret", "list"], &path, None);
    assert!(text(&out).contains("no stored secret"), "{}", text(&out));
}

/// A master key an earlier version kept in the data directory moves beside the configuration
/// at the next command, and the secrets still open with it.
#[tokio::test]
async fn a_key_in_the_data_directory_moves_beside_the_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let path = config(dir.path());
    let out = fronde(
        &[
            "secret",
            "set",
            "caldav",
            "--origin",
            "https://cloud.example",
        ],
        &path,
        Some(CANARY.as_bytes()),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let key_file = dir.path().join("master.key");
    let legacy = dir.path().join("data/master.key");
    std::fs::rename(&key_file, &legacy).unwrap();

    let out = fronde(&["secret", "list"], &path, None);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(shown.contains("moved the master key"), "{shown}");
    assert!(key_file.is_file() && !legacy.exists());
    assert_eq!(stored(&path, "caldav").await, CANARY);
}

#[test]
fn set_refuses_empty_input_and_bad_origins() {
    let dir = tempfile::tempdir().unwrap();
    let path = config(dir.path());
    let out = fronde(
        &[
            "secret",
            "set",
            "caldav",
            "--origin",
            "https://cloud.example",
        ],
        &path,
        Some(b"\n"),
    );
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(shown.contains("standard input is empty"), "{shown}");

    let out = fronde(
        &[
            "secret",
            "set",
            "caldav",
            "--origin",
            "https://cloud.example/path",
        ],
        &path,
        Some(CANARY.as_bytes()),
    );
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(shown.contains("scheme://host[:port]"), "{shown}");
    assert!(!shown.contains(CANARY), "{shown}");
    assert!(
        !dir.path().join("master.key").exists() && !dir.path().join("data/master.key").exists(),
        "a refused secret made a key"
    );
}

#[test]
fn cli_refuses_a_terminal() {
    let err = fronde::secret::read_value(&b"typed"[..], true)
        .unwrap_err()
        .to_string();
    assert!(err.contains("is a terminal"), "{err}");
}

#[test]
fn read_value_drops_one_line_break_only() {
    let read = |input: &[u8]| {
        fronde::secret::read_value(input, false)
            .map(|v| v.expose_secret().to_owned())
            .map_err(|e| e.to_string())
    };
    assert_eq!(read(b"value").unwrap(), "value");
    assert_eq!(read(b"value\n").unwrap(), "value");
    assert_eq!(read(b"value\r\n").unwrap(), "value");
    assert_eq!(read(b"value\n\n").unwrap(), "value\n");
    assert_eq!(read(b" spaced ").unwrap(), " spaced ");
    assert!(read(b"").unwrap_err().contains("empty"));
    assert!(read(b"\xff\xfe").unwrap_err().contains("not UTF-8"));
    let long = vec![b'a'; fronde_db::secrets::MAX_VALUE_LEN + 1];
    assert!(read(&long).unwrap_err().contains("longer than"));
    let max = vec![b'a'; fronde_db::secrets::MAX_VALUE_LEN];
    assert_eq!(read(&max).unwrap().len(), max.len());
}

/// With `auto`, `fronde secret set` makes the first key in the keychain when it answers, says
/// where, and leaves no key file; a later locked keychain makes it fail without a key file.
#[tokio::test]
async fn set_keeps_the_first_key_in_the_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = fronde_db::master_key::FakeKeychain::new();
    let config = auto_config(dir.path(), &keychain, None);
    let value = secrecy::SecretString::from(CANARY.to_owned());

    let mut out = Vec::new();
    fronde::secret::set(&config, "caldav", "https://cloud.example", &value, &mut out)
        .await
        .unwrap();
    let shown = String::from_utf8(out).unwrap();
    assert!(
        shown.contains("created the master key in the keychain (fake, entry fronde / master-key@"),
        "{shown}"
    );
    assert!(!shown.contains(CANARY), "{shown}");
    assert!(keychain.entry(&config.data.dir).is_some());
    assert!(!config.master_key_file().unwrap().exists(), "no key file");

    keychain.lock();
    let e = fronde::secret::set(
        &config,
        "other",
        "https://cloud.example",
        &value,
        &mut Vec::new(),
    )
    .await
    .unwrap_err();
    assert!(format!("{e:#}").contains("is locked"), "{e:#}");
    assert!(!config.master_key_file().unwrap().exists(), "no key file");
    keychain.unlock();
    assert_eq!(stored_with(&config, "caldav").await, CANARY);
}

/// Under a unit that loads the credential, `auto` reads the key from `$CREDENTIALS_DIRECTORY`
/// (here a temporary directory): no key is made, in the keychain or in a file.
#[tokio::test]
async fn set_reads_the_systemd_credential() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = tempfile::tempdir().unwrap();
    fronde_hub::private::write(
        credentials.path().join(fronde_db::master_key::CREDENTIAL),
        "fronde-key-v1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE\n",
    )
    .unwrap();
    let keychain = fronde_db::master_key::FakeKeychain::new();
    let config = auto_config(
        dir.path(),
        &keychain,
        Some(credentials.path().to_path_buf()),
    );
    let value = secrecy::SecretString::from(CANARY.to_owned());

    let mut out = Vec::new();
    fronde::secret::set(&config, "caldav", "https://cloud.example", &value, &mut out)
        .await
        .unwrap();
    let shown = String::from_utf8(out).unwrap();
    assert!(!shown.contains("created the master key"), "{shown}");
    assert!(keychain.entry(&config.data.dir).is_none());
    assert!(!config.master_key_file().unwrap().exists());
    assert_eq!(stored_with(&config, "caldav").await, CANARY);
}

/// The stored secret `name`, opened with the key from `config`'s backends.
async fn stored_with(config: &fronde_hub::config::Config, name: &str) -> String {
    let db = fronde_db::open(&config.data.dir).await.unwrap();
    let key = config
        .master_key_backends()
        .unwrap()
        .load(&db)
        .await
        .unwrap()
        .unwrap();
    let sealed = fronde_db::secrets::load_sealed(&db, fronde_db::secrets::SOURCES)
        .await
        .unwrap();
    let row = sealed.iter().find(|s| s.name == name).unwrap();
    row.open(&key).unwrap().expose_secret().to_owned()
}

#[tokio::test]
async fn the_agent_token_is_read_at_each_start() {
    use fronde_hub::config::{CLAUDE_ORIGIN, Secret};
    let dir = tempfile::tempdir().unwrap();
    let config = fronde_hub::config::load(&config(dir.path())).unwrap();
    fronde_hub::private::prepare_data_dir(&config.data.dir).unwrap();
    let db = fronde_db::open(&config.data.dir).await.unwrap();
    let backends = config.master_key_backends().unwrap();
    let read = |secret: Secret| fronde::agent_oauth_token(secret, db.clone(), backends.clone());

    // Stored for Claude's API: opened with the master key, again at each start.
    let stored = read(Secret::Stored {
        stored: "claude".into(),
    });
    let missing = stored().await.unwrap_err().to_string();
    assert!(
        missing.contains("fronde secret set claude --origin"),
        "{missing}"
    );
    for value in ["first-token", "second-token"] {
        fronde_db::secrets::store(
            &db,
            &backends,
            fronde_db::secrets::SOURCES,
            "claude",
            CLAUDE_ORIGIN,
            &value.to_owned().into(),
        )
        .await
        .unwrap();
        assert_eq!(stored().await.unwrap().expose_secret(), value);
    }
    // Stored for another origin, it is refused.
    fronde_db::secrets::store(
        &db,
        &backends,
        fronde_db::secrets::SOURCES,
        "elsewhere",
        "https://cloud.example",
        &"x".to_owned().into(),
    )
    .await
    .unwrap();
    let error = read(Secret::Stored {
        stored: "elsewhere".into(),
    })()
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("not Claude's API"), "{error}");

    // A file, read again at each start.
    let file = dir.path().join("claude-token");
    fronde_hub::private::write(&file, "from-file\n").unwrap();
    let from_file = read(Secret::File { file: file.clone() });
    assert_eq!(from_file().await.unwrap().expose_secret(), "from-file");
    fronde_hub::private::write(&file, "changed\n").unwrap();
    assert_eq!(from_file().await.unwrap().expose_secret(), "changed");
}

/// The key file's line without its prefix and line break: the key's bytes, as they would show.
fn key_material(config: &Path) -> String {
    let file = config.parent().unwrap().join("master.key");
    let line = std::fs::read_to_string(file).unwrap();
    line.trim_end()
        .strip_prefix("fronde-key-v1:")
        .unwrap()
        .to_owned()
}

fn set_from_cli(path: &Path, name: &str, value: &str) {
    let out = fronde(
        &["secret", "set", name, "--origin", "https://cloud.example"],
        path,
        Some(value.as_bytes()),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

/// `fronde secret rotate-key` seals every secret again with a new key in the key file, and shows
/// neither key nor any value.
#[tokio::test]
async fn rotate_key_from_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = config(dir.path());
    set_from_cli(&path, "caldav", CANARY);
    set_from_cli(&path, "other", "canary-cli-other-61b0");
    let old = key_material(&path);

    let out = fronde(&["secret", "rotate-key"], &path, None);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(0), "{shown}");
    assert!(shown.contains("rotated the master key"), "{shown}");
    assert!(shown.contains("2 stored secret(s) sealed again"), "{shown}");
    let new = key_material(&path);
    assert_ne!(new, old);
    for material in [&old, &new, &CANARY.to_owned()] {
        assert!(!shown.contains(material.as_str()), "{shown}");
    }
    assert!(!dir.path().join("master.key.next").exists());
    assert_eq!(stored(&path, "caldav").await, CANARY);
    assert_eq!(stored(&path, "other").await, "canary-cli-other-61b0");
}

/// While `fronde serve` runs on the data directory, rotating or moving the key is refused, naming
/// the process; once it stops, the rotation goes through.
#[tokio::test]
async fn rotate_key_is_refused_while_fronde_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = config(dir.path());
    set_from_cli(&path, "caldav", CANARY);
    let old = key_material(&path);
    let mut config = fronde_hub::config::load(&path).unwrap();
    config.server.listen = "127.0.0.1:0".parse().unwrap();
    let (_, server) = fronde::start(config).await.unwrap();

    for args in [
        &["secret", "rotate-key"][..],
        &["secret", "move-key", "--to", "file"],
    ] {
        let out = fronde(args, &path, None);
        let shown = text(&out);
        assert_ne!(out.status.code(), Some(0), "{shown}");
        assert!(
            shown.contains(&format!("process {} (fronde serve)", std::process::id())),
            "{shown}"
        );
        assert!(shown.contains("Stop it first"), "{shown}");
    }
    assert_eq!(key_material(&path), old, "nothing changed");

    server.abort();
    let _ = server.await;
    let out = fronde(&["secret", "rotate-key"], &path, None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_ne!(key_material(&path), old);
    assert_eq!(stored(&path, "caldav").await, CANARY);
}

/// A rotation that stopped after sealing the secrets with the new key, before that key replaced
/// the old one in the file, is finished when Fronde starts.
#[tokio::test]
async fn serve_finishes_an_interrupted_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = config(dir.path());
    set_from_cli(&path, "caldav", CANARY);
    let old = key_material(&path);
    let mut config = fronde_hub::config::load(&path).unwrap();
    {
        let db = fronde_db::open(&config.data.dir).await.unwrap();
        let backends = config.master_key_backends().unwrap();
        fronde_db::master_key::rotate_crashing_after(
            &db,
            &backends,
            fronde_db::master_key::Step::Resealed,
        )
        .await
        .unwrap_err();
        db.close().await.unwrap();
    }
    assert_eq!(
        key_material(&path),
        old,
        "the old key is still the current one"
    );
    assert!(dir.path().join("master.key.next").exists());

    config.server.listen = "127.0.0.1:0".parse().unwrap();
    let (_, server) = fronde::start(config).await.unwrap();
    server.abort();
    let _ = server.await;
    assert_ne!(key_material(&path), old, "the new key was promoted");
    assert!(!dir.path().join("master.key.next").exists());
    assert_eq!(stored(&path, "caldav").await, CANARY);
}

/// `fronde secret move-key` moves the key from the file to the keychain and back, saying where
/// without showing it; a keychain that cannot take it, or a configuration that names the file
/// backend, changes nothing.
#[tokio::test]
async fn move_key_between_the_file_and_the_keychain() {
    let dir = tempfile::tempdir().unwrap();
    let keychain = fronde_db::master_key::FakeKeychain::new();
    let path = config(dir.path());
    set_from_cli(&path, "caldav", CANARY);
    let material = key_material(&path);
    let key_file = dir.path().join("master.key");

    // `[secrets] backend = "file"`: the key would be looked for there.
    let named = fronde::secret::move_key(
        &fronde_hub::config::load(&path).unwrap(),
        "keychain",
        &mut Vec::new(),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{named:#}").contains("backend = \"file\""),
        "{named:#}"
    );
    let unknown = fronde::secret::move_key(
        &fronde_hub::config::load(&path).unwrap(),
        "vault",
        &mut Vec::new(),
    )
    .await
    .unwrap_err();
    assert!(format!("{unknown:#}").contains("file, keychain or systemd"));

    let config = auto_config(dir.path(), &keychain, None);
    keychain.lock();
    let locked = fronde::secret::move_key(&config, "keychain", &mut Vec::new())
        .await
        .unwrap_err();
    assert!(format!("{locked:#}").contains("locked"), "{locked:#}");
    keychain.unlock();
    assert_eq!(key_material(&path), material, "nothing changed");
    assert!(keychain.entry(&config.data.dir).is_none());

    let mut out = Vec::new();
    fronde::secret::move_key(&config, "keychain", &mut out)
        .await
        .unwrap();
    let shown = String::from_utf8(out).unwrap();
    assert!(shown.contains("moved the master key from"), "{shown}");
    assert!(shown.contains("to the keychain (fake"), "{shown}");
    assert!(shown.contains("removed it from"), "{shown}");
    assert!(
        !shown.contains(&material) && !shown.contains(CANARY),
        "{shown}"
    );
    assert!(!key_file.exists());
    assert!(
        keychain
            .entry(&config.data.dir)
            .unwrap()
            .contains(&material)
    );
    assert_eq!(stored_with(&config, "caldav").await, CANARY);

    let mut out = Vec::new();
    fronde::secret::move_key(&config, "file", &mut out)
        .await
        .unwrap();
    let shown = String::from_utf8(out).unwrap();
    assert!(
        !shown.contains(&material) && !shown.contains(CANARY),
        "{shown}"
    );
    assert_eq!(key_material(&path), material);
    assert!(keychain.entry(&config.data.dir).is_none());
    assert_eq!(stored_with(&config, "caldav").await, CANARY);
}
