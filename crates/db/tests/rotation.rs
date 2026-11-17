//! Rotating the master key and moving it between backends: every secret opens afterwards, and
//! after a crash at any step once the next start has finished what was interrupted. The keychain
//! is [`FakeKeychain`], on keyring-core's mock store: no test touches a real keychain.

use fronde_db::master_key::{
    self, Backend, Backends, Choice, FakeKeychain, Finished, MasterKey, Step,
};
use fronde_db::secrets::{self, SOURCES};
use fronde_entity::secrets as rows;
use sea_orm::EntityTrait;
use secrecy::{ExposeSecret as _, SecretString};
use std::path::PathBuf;

const ORIGIN: &str = "https://cloud.example";
/// The secrets every test stores, in two scopes: a rotation seals every scope again.
const STORED: [(&str, &str, &str); 3] = [
    (SOURCES, "caldav", "canary-rotation-caldav-41a7"),
    (SOURCES, "token", "canary-rotation-token-9c2e"),
    ("other", "mail", "canary-rotation-mail-d03b"),
];

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

    fn next_file(&self) -> PathBuf {
        self.config.join("master.key.next")
    }

    fn backends(&self, choice: Choice) -> Backends {
        Backends::new(
            choice,
            self.key_file(),
            Some(self.credentials.clone()),
            self.keychain.keychain(&self.data),
        )
    }

    /// The key's backends after a restart: a new handle on the same keychain, as a new process
    /// has.
    fn auto(&self) -> Backends {
        self.backends(Choice::Auto)
    }

    /// Stores [`STORED`] under a first key made in `backend`.
    async fn store_all(&self, backend: Backend) {
        let backends = self.backends(Choice::Only(backend));
        for (scope, name, value) in STORED {
            secrets::store(
                &self.db,
                &backends,
                scope,
                name,
                ORIGIN,
                &SecretString::from(value),
            )
            .await
            .unwrap();
        }
    }

    /// The key that opens the stored secrets now, as `fronde serve` loads it.
    async fn key(&self) -> std::sync::Arc<MasterKey> {
        self.auto().load(&self.db).await.unwrap().expect("a key")
    }

    /// Whether every stored secret opens with `key` and holds its value.
    async fn all_open(&self, key: &MasterKey) -> bool {
        let mut opened = 0;
        for scope in [SOURCES, "other"] {
            for sealed in secrets::load_sealed(&self.db, scope).await.unwrap() {
                let Ok(value) = sealed.open(key) else {
                    return false;
                };
                let expected = STORED
                    .iter()
                    .find(|(s, n, _)| *s == scope && *n == sealed.name)
                    .unwrap()
                    .2;
                if value.expose_secret() != expected {
                    return false;
                }
                opened += 1;
            }
        }
        opened == STORED.len()
    }

    /// The key's line where `backend` keeps it.
    fn line(&self, backend: Backend) -> Option<String> {
        match backend {
            Backend::File => std::fs::read_to_string(self.key_file()).ok(),
            Backend::Keychain => self.keychain.entry(&self.data),
            Backend::Systemd => {
                std::fs::read_to_string(self.credentials.join(master_key::CREDENTIAL)).ok()
            }
        }
    }

    fn next_line(&self, backend: Backend) -> Option<String> {
        match backend {
            Backend::File => std::fs::read_to_string(self.next_file()).ok(),
            Backend::Keychain => self.keychain.next_entry(&self.data),
            Backend::Systemd => None,
        }
    }

    async fn rows(&self) -> Vec<rows::Model> {
        rows::Entity::find().all(&self.db).await.unwrap()
    }
}

fn err<T: std::fmt::Debug>(result: anyhow::Result<T>) -> String {
    format!("{:#}", result.unwrap_err())
}

/// One key line, trimmed, for comparisons.
fn trimmed(line: Option<String>) -> String {
    line.expect("a key line").trim_end().to_owned()
}

#[tokio::test]
async fn rotation_reencrypts_every_row() {
    for backend in [Backend::File, Backend::Keychain] {
        let s = Setup::new().await;
        s.store_all(backend).await;
        let old = s.key().await;
        let old_line = trimmed(s.line(backend));
        let before = s.rows().await;

        let rotated = master_key::rotate(&s.db, &s.auto()).await.unwrap();
        assert_eq!(rotated.secrets, STORED.len(), "{backend}");

        let new = s.key().await;
        let new_line = trimmed(s.line(backend));
        assert_ne!(new_line, old_line, "{backend}: the key changed");
        assert_eq!(
            new.backend(),
            backend,
            "{backend}: the key stays where it was"
        );
        assert!(s.all_open(&new).await, "{backend}");
        assert!(
            !s.all_open(&old).await,
            "{backend}: the old key opens nothing"
        );
        assert!(secrets::check_key(&s.db, &old).await.is_err(), "{backend}");
        // Every row sealed again, under a fresh nonce.
        let after = s.rows().await;
        assert_eq!(after.len(), before.len());
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(
                (&b.scope, &b.name, &b.origin),
                (&a.scope, &a.name, &a.origin)
            );
            assert_ne!(b.nonce, a.nonce, "{backend}: {}", a.name);
            assert_ne!(b.ciphertext, a.ciphertext, "{backend}: {}", a.name);
            assert_eq!(b.updated_at, a.updated_at, "the values did not change");
        }
        assert_eq!(
            secrets::recorded_backend(&s.db).await.unwrap(),
            Some(backend)
        );
        assert!(
            s.next_line(backend).is_none(),
            "{backend}: no next key left"
        );
        assert_eq!(
            master_key::finish(&s.db, &s.auto()).await.unwrap(),
            None,
            "no rotation left under way"
        );
        // A secret stored now is sealed with the new key.
        secrets::store(
            &s.db,
            &s.auto(),
            SOURCES,
            "later",
            ORIGIN,
            &SecretString::from("later"),
        )
        .await
        .unwrap();
        assert_eq!(trimmed(s.line(backend)), new_line);
    }
}

#[tokio::test]
async fn rotation_survives_a_crash_at_each_step() {
    for backend in [Backend::File, Backend::Keychain] {
        for step in Step::ROTATION {
            let s = Setup::new().await;
            s.store_all(backend).await;
            let old_line = trimmed(s.line(backend));

            let crashed = master_key::rotate_crashing_after(&s.db, &s.auto(), step).await;
            let shown = err(crashed);
            assert!(
                shown.contains("as a crash would"),
                "{backend} {step:?}: {shown}"
            );

            // The next start.
            let finished = master_key::finish(&s.db, &s.auto()).await.unwrap();
            let sealed_with_new = matches!(step, Step::Resealed | Step::Promoted | Step::Cleaned);
            let expected = match step {
                Step::Marked | Step::Cleaned => Finished::Cleared,
                Step::Staged => Finished::Undone,
                Step::Resealed => Finished::Promoted,
                // The new key is current already; only the next slot is left.
                Step::Promoted => Finished::Cleared,
                other => unreachable!("{other:?}"),
            };
            assert_eq!(finished, Some(expected), "{backend} {step:?}");

            let key = s.key().await;
            assert!(s.all_open(&key).await, "{backend} {step:?}");
            let line = trimmed(s.line(backend));
            assert_eq!(
                line != old_line,
                sealed_with_new,
                "{backend} {step:?}: the key in use"
            );
            assert!(s.next_line(backend).is_none(), "{backend} {step:?}");
            assert_eq!(
                master_key::finish(&s.db, &s.auto()).await.unwrap(),
                None,
                "{backend} {step:?}: finished once"
            );

            // And a whole rotation goes through after it.
            let rotated = master_key::rotate(&s.db, &s.auto()).await.unwrap();
            assert_eq!(rotated.secrets, STORED.len());
            let key = s.key().await;
            assert!(s.all_open(&key).await, "{backend} {step:?}: rotated again");
        }
    }
}

/// Between the commit and the promotion, the current key no longer opens the secrets: what loads
/// the key says that a rotation was interrupted and what finishes it, and the next rotation or
/// start finishes it.
#[tokio::test]
async fn an_interrupted_rotation_is_named_and_finished_by_the_next_one() {
    let s = Setup::new().await;
    s.store_all(Backend::File).await;
    master_key::rotate_crashing_after(&s.db, &s.auto(), Step::Resealed)
        .await
        .unwrap_err();
    let shown = err(s.auto().load(&s.db).await);
    assert!(
        shown.contains("rotation of the master key was interrupted"),
        "{shown}"
    );
    assert!(shown.contains("fronde secret rotate-key"), "{shown}");
    let rotated = master_key::rotate(&s.db, &s.auto()).await.unwrap();
    assert_eq!(rotated.secrets, STORED.len());
    assert!(s.all_open(&*s.key().await).await);
    assert!(s.next_line(Backend::File).is_none());
}

#[tokio::test]
async fn move_key_between_file_and_fake_keychain() {
    let s = Setup::new().await;
    s.store_all(Backend::File).await;
    let line = trimmed(s.line(Backend::File));

    let moved = master_key::move_to(&s.db, &s.auto(), Backend::Keychain)
        .await
        .unwrap();
    assert_eq!((moved.from, moved.to), (Backend::File, Backend::Keychain));
    assert!(moved.removed);
    assert_eq!(trimmed(s.line(Backend::Keychain)), line, "the same key");
    assert!(!s.key_file().exists(), "removed from the file");
    assert_eq!(
        secrets::recorded_backend(&s.db).await.unwrap(),
        Some(Backend::Keychain)
    );
    assert!(
        secrets::recorded_keychain_entry(&s.db)
            .await
            .unwrap()
            .is_some()
    );
    let key = s.key().await;
    assert_eq!(key.backend(), Backend::Keychain);
    assert!(s.all_open(&key).await);

    let moved = master_key::move_to(&s.db, &s.auto(), Backend::File)
        .await
        .unwrap();
    assert_eq!((moved.from, moved.to), (Backend::Keychain, Backend::File));
    assert_eq!(trimmed(s.line(Backend::File)), line);
    assert!(
        s.line(Backend::Keychain).is_none(),
        "removed from the keychain"
    );
    assert_eq!(
        secrets::recorded_backend(&s.db).await.unwrap(),
        Some(Backend::File)
    );
    assert_eq!(secrets::recorded_keychain_entry(&s.db).await.unwrap(), None);
    let key = s.key().await;
    assert_eq!(key.backend(), Backend::File);
    assert!(s.all_open(&key).await);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(s.key_file())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[tokio::test]
async fn move_key_to_a_locked_keychain_changes_nothing() {
    let s = Setup::new().await;
    s.store_all(Backend::File).await;
    let line = trimmed(s.line(Backend::File));
    s.keychain.lock();
    let shown = err(master_key::move_to(&s.db, &s.auto(), Backend::Keychain).await);
    assert!(shown.contains("locked"), "{shown}");
    s.keychain.make_unreachable();
    let shown = err(master_key::move_to(&s.db, &s.auto(), Backend::Keychain).await);
    assert!(shown.contains("cannot be reached"), "{shown}");
    s.keychain.unlock();
    assert_eq!(trimmed(s.line(Backend::File)), line);
    assert!(s.line(Backend::Keychain).is_none());
    assert_eq!(
        secrets::recorded_backend(&s.db).await.unwrap(),
        Some(Backend::File)
    );
    assert!(s.all_open(&*s.key().await).await);
}

#[tokio::test]
async fn move_survives_a_crash_at_each_step() {
    for step in Step::MOVE {
        let s = Setup::new().await;
        s.store_all(Backend::File).await;
        let shown =
            err(master_key::move_crashing_after(&s.db, &s.auto(), Backend::Keychain, step).await);
        assert!(shown.contains("as a crash would"), "{step:?}: {shown}");
        // The key is in both places, and the record names one of them.
        assert!(s.line(Backend::File).is_some());
        assert!(s.line(Backend::Keychain).is_some());
        let key = s.key().await;
        let expected = match step {
            Step::Copied => Backend::File,
            _ => Backend::Keychain,
        };
        assert_eq!(key.backend(), expected, "{step:?}");
        assert!(s.all_open(&key).await, "{step:?}");
        // Moving again ends where the uninterrupted move would.
        match step {
            Step::Copied => {
                master_key::move_to(&s.db, &s.auto(), Backend::Keychain)
                    .await
                    .unwrap();
                assert!(!s.key_file().exists());
            }
            _ => {
                let shown = err(master_key::move_to(&s.db, &s.auto(), Backend::Keychain).await);
                assert!(shown.contains("already kept in the keychain"), "{shown}");
            }
        }
        assert!(s.all_open(&*s.key().await).await, "{step:?}");
    }
}

#[tokio::test]
async fn move_and_rotation_refusals_change_nothing() {
    let s = Setup::new().await;
    // Nothing stored: nothing to rotate or move.
    let shown = err(master_key::rotate(&s.db, &s.auto()).await);
    assert!(shown.contains("no secret is stored"), "{shown}");
    let shown = err(master_key::move_to(&s.db, &s.auto(), Backend::Keychain).await);
    assert!(shown.contains("no secret is stored"), "{shown}");

    s.store_all(Backend::File).await;
    let line = trimmed(s.line(Backend::File));
    // A configuration that names another backend would keep reading the key there.
    let shown = err(master_key::move_to(
        &s.db,
        &s.backends(Choice::Only(Backend::File)),
        Backend::Keychain,
    )
    .await);
    assert!(shown.contains("[secrets] backend = \"file\""), "{shown}");
    // Fronde never writes a systemd credential.
    let shown = err(master_key::move_to(&s.db, &s.auto(), Backend::Systemd).await);
    assert!(shown.contains("systemd-creds encrypt"), "{shown}");
    // A keychain entry that holds another key is never replaced.
    s.keychain.put(
        &s.data,
        "fronde-key-v1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE",
    );
    let shown = err(master_key::move_to(&s.db, &s.auto(), Backend::Keychain).await);
    assert!(
        shown.contains("already holds another master key"),
        "{shown}"
    );
    assert_eq!(trimmed(s.line(Backend::File)), line);
    assert_eq!(
        secrets::recorded_backend(&s.db).await.unwrap(),
        Some(Backend::File)
    );
    assert!(s.all_open(&*s.key().await).await);

    // A key in a systemd credential is rotated elsewhere.
    let s = Setup::new().await;
    std::fs::write(
        s.credentials.join(master_key::CREDENTIAL),
        "fronde-key-v1:AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI\n",
    )
    .unwrap();
    s.store_all(Backend::Systemd).await;
    let shown = err(master_key::rotate(&s.db, &s.auto()).await);
    assert!(shown.contains("never writes"), "{shown}");
    assert!(s.next_line(Backend::File).is_none());
    assert_eq!(master_key::finish(&s.db, &s.auto()).await.unwrap(), None);
    assert!(s.all_open(&*s.key().await).await);
}

#[test]
fn a_running_fronde_holds_the_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let held = fronde_db::running::claim(dir.path(), "fronde serve").unwrap();
    let shown = err(fronde_db::running::claim(
        dir.path(),
        "fronde secret rotate-key",
    ));
    assert!(
        shown.contains(&format!("process {} (fronde serve)", std::process::id())),
        "{shown}"
    );
    assert!(shown.contains("Stop it first"), "{shown}");
    drop(held);
    let _again = fronde_db::running::claim(dir.path(), "fronde secret rotate-key").unwrap();
}
