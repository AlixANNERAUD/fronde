//! Where the master key is kept: the order `auto` follows for the first key, the record that
//! keeps that choice, and a keychain that is locked or unreachable. The keychain is
//! [`FakeKeychain`], on keyring-core's mock store; the systemd credentials directory is a
//! temporary directory. No test touches a real keychain or systemd.

use fronde_db::master_key::{Backend, Backends, CREDENTIAL, Choice, FakeKeychain, MasterKey};
use fronde_db::secrets::{self, SOURCES};
use secrecy::{ExposeSecret as _, SecretString};
use std::path::PathBuf;

const ORIGIN: &str = "https://cloud.example";
/// 32 bytes of 0x01.
const LINE: &str = "fronde-key-v1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE";

struct Setup {
    _root: tempfile::TempDir,
    data: PathBuf,
    config: PathBuf,
    credentials: PathBuf,
    db: fronde_db::DatabaseConnection,
    keychain: FakeKeychain,
}

impl Setup {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let [data, config, credentials] =
            ["data", "config", "credentials"].map(|d| root.path().join(d));
        for dir in [&data, &config, &credentials] {
            std::fs::create_dir(dir).unwrap();
        }
        let db = fronde_db::open(&data).await.unwrap();
        Self {
            _root: root,
            data,
            config,
            credentials,
            db,
            keychain: FakeKeychain::new(),
        }
    }

    fn key_file(&self) -> PathBuf {
        self.config.join("master.key")
    }

    /// The backends for `choice`, with the credentials directory when `systemd` says the
    /// service loads credentials.
    fn backends(&self, choice: Choice, systemd: bool) -> Backends {
        Backends::new(
            choice,
            self.key_file(),
            systemd.then(|| self.credentials.clone()),
            self.keychain.keychain(&self.data),
        )
    }

    fn auto(&self) -> Backends {
        self.backends(Choice::Auto, false)
    }

    fn put_credential(&self, line: &str) {
        fronde_fs::write(self.credentials.join(CREDENTIAL), format!("{line}\n")).unwrap();
    }

    async fn store(&self, key: &MasterKey, name: &str) {
        secrets::set(
            &self.db,
            key,
            SOURCES,
            name,
            ORIGIN,
            &SecretString::from("v"),
        )
        .await
        .unwrap();
    }

    async fn recorded(&self) -> Option<Backend> {
        secrets::recorded_backend(&self.db).await.unwrap()
    }

    /// The stored secret `name`, opened with `key`.
    async fn opens(&self, key: &MasterKey, name: &str) -> bool {
        let sealed = secrets::load_sealed(&self.db, SOURCES).await.unwrap();
        let row = sealed.iter().find(|s| s.name == name).unwrap();
        row.open(key).is_ok_and(|v| v.expose_secret() == "v")
    }
}

async fn err(result: anyhow::Result<impl std::fmt::Debug>) -> String {
    format!("{:#}", result.unwrap_err())
}

/// A systemd credential is an operator's explicit choice: with one, `auto` reads the key there,
/// makes none, and records it.
#[tokio::test]
async fn auto_prefers_systemd_credential() {
    let s = Setup::new().await;
    s.put_credential(LINE);
    let backends = s.backends(Choice::Auto, true);

    assert!(backends.load(&s.db).await.unwrap().is_none());
    let (key, created) = backends.load_or_create(&s.db).await.unwrap();
    assert!(!created, "systemd's key is read, not made");
    assert_eq!(key.backend(), Backend::Systemd);
    assert!(
        s.keychain.entry(&s.data).is_none(),
        "nothing in the keychain"
    );
    assert!(!s.key_file().exists(), "no key file");

    s.store(&key, "caldav").await;
    assert_eq!(s.recorded().await, Some(Backend::Systemd));
    let key = backends.load(&s.db).await.unwrap().unwrap();
    assert_eq!(key.backend(), Backend::Systemd);
    assert!(s.opens(&key, "caldav").await);
    let expected = MasterKey::parse(LINE.as_bytes(), Backend::File).unwrap();
    assert!(
        s.opens(&expected, "caldav").await,
        "the credential's key sealed it"
    );
}

/// Without a systemd credential, an unlocked keychain gets the first key, and keeps it.
#[tokio::test]
async fn auto_uses_keychain_when_unlocked() {
    let s = Setup::new().await;
    let (key, created) = s.auto().load_or_create(&s.db).await.unwrap();
    assert!(created);
    assert_eq!(key.backend(), Backend::Keychain);
    let entry = s.keychain.entry(&s.data).expect("an entry");
    assert!(entry.starts_with("fronde-key-v1:"), "the key line format");
    assert!(!s.key_file().exists(), "no key file");

    s.store(&key, "caldav").await;
    assert_eq!(s.recorded().await, Some(Backend::Keychain));
    // A systemd credential that shows up later does not change where the key is.
    s.put_credential(LINE);
    let backends = s.backends(Choice::Auto, true);
    let loaded = backends.load(&s.db).await.unwrap().unwrap();
    assert_eq!(loaded.backend(), Backend::Keychain);
    assert!(s.opens(&loaded, "caldav").await);
    let (_, created) = backends.load_or_create(&s.db).await.unwrap();
    assert!(!created);
    assert_eq!(s.keychain.entry(&s.data).unwrap(), entry, "the key is kept");
}

/// With no keychain to reach, the first key goes to a file, and stays there once the keychain
/// can be reached.
#[tokio::test]
async fn auto_falls_back_to_file_on_first_use_only() {
    let s = Setup::new().await;
    s.keychain.make_unreachable();
    let (key, created) = s.auto().load_or_create(&s.db).await.unwrap();
    assert!(created);
    assert_eq!(key.backend(), Backend::File);
    assert!(s.key_file().is_file());
    s.store(&key, "caldav").await;
    assert_eq!(s.recorded().await, Some(Backend::File));

    s.keychain.unlock();
    let loaded = s.auto().load(&s.db).await.unwrap().unwrap();
    assert_eq!(loaded.backend(), Backend::File);
    let (key, created) = s.auto().load_or_create(&s.db).await.unwrap();
    assert!(!created);
    assert_eq!(key.backend(), Backend::File);
    s.store(&key, "other").await;
    assert!(
        s.keychain.entry(&s.data).is_none(),
        "the keychain stays empty"
    );
}

/// A keychain that is locked at start, as after a reboot before the desktop session unlocks it,
/// stops Fronde with what to do. No key is made elsewhere, so no secret is stranded, and the key
/// is there again once the keychain is unlocked.
#[tokio::test]
async fn locked_keychain_never_falls_back() {
    let s = Setup::new().await;
    let (key, _) = s.auto().load_or_create(&s.db).await.unwrap();
    s.store(&key, "caldav").await;
    let entry = s.keychain.entry(&s.data).unwrap();

    s.keychain.lock();
    for e in [
        err(s.auto().load(&s.db).await).await,
        err(s.auto().load_or_create(&s.db).await).await,
    ] {
        assert!(e.contains("is locked"), "{e}");
        assert!(e.contains("unlock it"), "{e}");
        assert!(
            e.contains("the keychain (fake, entry fronde / master-key@"),
            "{e}"
        );
        assert!(!e.contains(&entry), "the error quotes the key: {e}");
    }
    assert!(!s.key_file().exists(), "no key file was made");
    assert_eq!(s.recorded().await, Some(Backend::Keychain));

    s.keychain.unlock();
    let key = s.auto().load(&s.db).await.unwrap().unwrap();
    assert!(s.opens(&key, "caldav").await);
    assert_eq!(s.keychain.entry(&s.data).unwrap(), entry);
}

/// A keychain that cannot be reached at all says so, and names the other backends.
#[tokio::test]
async fn unreachable_backend_explains() {
    let s = Setup::new().await;
    let (key, _) = s.auto().load_or_create(&s.db).await.unwrap();
    s.store(&key, "caldav").await;

    s.keychain.make_unreachable();
    let e = err(s.auto().load(&s.db).await).await;
    assert!(e.contains("cannot be reached"), "{e}");
    assert!(
        e.contains("[secrets] backend = \"systemd\" or \"file\""),
        "{e}"
    );
    assert!(!s.key_file().exists());
}

/// The backend is kept for good: once the last secret is removed, an unreachable keychain does
/// not stop a start (nothing needs the key), and the next first secret still goes to the
/// keychain, never to a file.
#[tokio::test]
async fn the_recorded_backend_outlives_the_last_secret() {
    let s = Setup::new().await;
    let (key, _) = s.auto().load_or_create(&s.db).await.unwrap();
    s.store(&key, "caldav").await;
    assert!(secrets::remove(&s.db, SOURCES, "caldav").await.unwrap());
    assert!(!secrets::any_stored(&s.db).await.unwrap());

    s.keychain.lock();
    assert!(s.auto().load(&s.db).await.unwrap().is_none());
    let e = err(s.auto().load_or_create(&s.db).await).await;
    assert!(e.contains("is locked"), "{e}");
    assert!(!s.key_file().exists(), "no key file was made");

    s.keychain.unlock();
    let (again, created) = s.auto().load_or_create(&s.db).await.unwrap();
    assert!(!created, "the keychain's key is used again");
    assert_eq!(again.backend(), Backend::Keychain);
}

/// While secrets are stored, a key from another backend is refused inside the write
/// transaction, and the record stays.
#[tokio::test]
async fn a_key_from_a_second_backend_never_seals() {
    let s = Setup::new().await;
    s.keychain.make_unreachable();
    let (key, _) = s.auto().load_or_create(&s.db).await.unwrap();
    s.store(&key, "caldav").await;

    let other = MasterKey::generate(Backend::Keychain).unwrap();
    let e = secrets::set(
        &s.db,
        &other,
        SOURCES,
        "x",
        ORIGIN,
        &SecretString::from("v"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("is not the one that sealed"), "{e}");
    assert_eq!(s.recorded().await, Some(Backend::File));
}

/// A key in the keychain that did not seal the stored secrets is reported as such, naming the
/// keychain entry.
#[tokio::test]
async fn a_wrong_key_in_the_keychain_is_reported() {
    let s = Setup::new().await;
    let (key, _) = s.auto().load_or_create(&s.db).await.unwrap();
    s.store(&key, "caldav").await;
    s.keychain.put(&s.data, LINE);
    let e = err(s.auto().load(&s.db).await).await;
    assert!(e.contains("is not the one that sealed"), "{e}");
    assert!(e.contains("the keychain (fake"), "{e}");
    assert!(!e.contains("AQEB"), "the error quotes the key: {e}");
}

/// A named backend is used alone: never the order, never another one.
#[tokio::test]
async fn an_explicit_backend_is_used_alone() {
    let s = Setup::new().await;
    s.put_credential(LINE);
    let (key, created) = s
        .backends(Choice::Only(Backend::File), true)
        .load_or_create(&s.db)
        .await
        .unwrap();
    assert!(created);
    assert_eq!(key.backend(), Backend::File);
    assert!(s.key_file().is_file());
    assert!(s.keychain.entry(&s.data).is_none());

    let s = Setup::new().await;
    s.keychain.make_unreachable();
    let e = err(s
        .backends(Choice::Only(Backend::Keychain), false)
        .load_or_create(&s.db)
        .await)
    .await;
    assert!(e.contains("cannot be reached"), "{e}");
    assert!(!s.key_file().exists());

    let e = err(s
        .backends(Choice::Only(Backend::Systemd), false)
        .load_or_create(&s.db)
        .await)
    .await;
    assert!(e.contains("$CREDENTIALS_DIRECTORY is not set"), "{e}");
    assert!(e.contains("systemd-run"), "{e}");
    let e = err(s
        .backends(Choice::Only(Backend::Systemd), true)
        .load_or_create(&s.db)
        .await)
    .await;
    assert!(
        e.contains("systemd-creds encrypt --name=fronde-master-key"),
        "{e}"
    );
    assert!(!s.key_file().exists());
}

/// The credential is read as systemd leaves it (read-only), and a malformed one is refused
/// without quoting it.
#[cfg(unix)]
#[tokio::test]
async fn systemd_credential_is_read_from_the_credentials_directory() {
    use std::os::unix::fs::PermissionsExt as _;
    let s = Setup::new().await;
    s.put_credential(LINE);
    let file = s.credentials.join(CREDENTIAL);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
    let backends = s.backends(Choice::Only(Backend::Systemd), true);
    let (key, created) = backends.load_or_create(&s.db).await.unwrap();
    assert!(!created);
    s.store(&key, "caldav").await;
    let expected = MasterKey::parse(LINE.as_bytes(), Backend::File).unwrap();
    assert!(s.opens(&expected, "caldav").await);

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    fronde_fs::write(&file, "fronde-key-v1:AQEB\n").unwrap();
    let e = err(backends.load(&s.db).await).await;
    assert!(e.contains(&file.display().to_string()), "{e}");
    assert!(e.contains("not a Fronde master key"), "{e}");
    assert!(!e.contains("AQEB"), "the error quotes the key: {e}");

    // The record makes `auto` read the credential too, and a missing one is never replaced.
    std::fs::remove_file(&file).unwrap();
    let e = err(s.backends(Choice::Auto, true).load(&s.db).await).await;
    assert!(e.contains("there is no systemd credential"), "{e}");
    assert!(
        e.contains("holds secrets sealed with the master key"),
        "{e}"
    );
    assert!(!s.key_file().exists());
    assert!(s.keychain.entry(&s.data).is_none());
}

/// Each data directory has its own keychain entry: two Fronde instances never share a key.
#[tokio::test]
async fn keychain_entry_is_per_data_dir() {
    let s = Setup::new().await;
    let other = s._root.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let a = s.keychain.keychain(&s.data);
    let b = s.keychain.keychain(&other);
    assert_ne!(a.user(), b.user());
    assert!(a.user().starts_with("master-key@"), "{}", a.user());

    s.auto().load_or_create(&s.db).await.unwrap();
    let db = fronde_db::open(&other).await.unwrap();
    let backends = Backends::new(Choice::Auto, other.join("master.key"), None, b);
    backends.load_or_create(&db).await.unwrap();
    assert_ne!(s.keychain.entry(&s.data), s.keychain.entry(&other));
}

/// A database from before backends were recorded has its secrets sealed by the key file: `auto`
/// reads that file and never looks for the key in the keychain, and the next start records it.
#[tokio::test]
async fn secrets_stored_before_the_record_use_the_key_file() {
    let s = Setup::new().await;
    let (key, _) = Backends::file(s.key_file())
        .load_or_create(&s.db)
        .await
        .unwrap();
    s.store(&key, "caldav").await;
    assert!(
        fronde_db::kv::delete(&s.db, "secrets", "backend")
            .await
            .unwrap()
    );

    let key = s.auto().load(&s.db).await.unwrap().unwrap();
    assert_eq!(key.backend(), Backend::File);
    assert!(s.opens(&key, "caldav").await);
    let (_, created) = s.auto().load_or_create(&s.db).await.unwrap();
    assert!(!created);
    assert!(s.keychain.entry(&s.data).is_none());

    assert!(s.recorded().await.is_none());
    secrets::upgrade_key_check(&s.db, &key).await.unwrap();
    assert_eq!(s.recorded().await, Some(Backend::File));
}

/// A second first use, before any secret is stored, reads the key the first one made.
#[tokio::test]
async fn the_keychain_key_is_made_once() {
    let s = Setup::new().await;
    let backends = s.auto();
    let (a, created_a) = backends.load_or_create(&s.db).await.unwrap();
    let (b, created_b) = backends.load_or_create(&s.db).await.unwrap();
    assert!(created_a && !created_b);
    s.store(&a, "caldav").await;
    assert!(s.opens(&b, "caldav").await);
}
