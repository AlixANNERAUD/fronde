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
