use std::fs;

use tempfile::TempDir;

use super::*;

async fn store(dir: &TempDir) -> Store {
    Store::open(&dir.path().join("state")).await.unwrap()
}

fn name(s: &str) -> SecretName {
    s.parse().unwrap()
}

#[test]
fn secret_names() {
    for ok in [
        "notes/database-url",
        "notes/S3_KEY.v2",
        "hoster/registry/ghcr.io",
    ] {
        assert!(ok.parse::<SecretName>().is_ok(), "{ok}");
    }
    for bad in [
        "",
        "/x",
        "x/",
        "a//b",
        "a/../b",
        "a/./b",
        "a b",
        "a:b",
        &"x".repeat(256),
    ] {
        assert!(bad.parse::<SecretName>().is_err(), "{bad:?}");
    }
}

#[test]
fn values_and_keys_never_print() {
    let value = SecretValue::from("hunter2");
    assert_eq!(format!("{value:?}"), "<redacted>");
    let key = SecretKey::generate();
    assert!(!format!("{key:?}").contains(&BASE64.encode(key.0)));
}

#[tokio::test]
async fn set_reveal_replace_remove_and_list() {
    let dir = TempDir::new().unwrap();
    let secrets = SecretStore::new(store(&dir).await, &SecretKey::generate());
    let db = name("notes/database-url");

    assert!(secrets.reveal(db.as_ref()).await.unwrap().is_none());
    assert!(secrets.version(db.as_ref()).await.unwrap().is_none());

    let v1 = secrets.set(&db, &"postgres://a".into()).await.unwrap();
    let (value, version) = secrets.reveal(db.as_ref()).await.unwrap().unwrap();
    assert_eq!(value.expose(), "postgres://a");
    assert_eq!(version, v1);
    assert_eq!(
        secrets.version(db.as_ref()).await.unwrap(),
        Some(v1.clone())
    );

    // Replacing gives a new version, even for the same value.
    let v2 = secrets.set(&db, &"postgres://a".into()).await.unwrap();
    assert_ne!(v1, v2);

    secrets
        .set(&name("notes/s3-key"), &"k".into())
        .await
        .unwrap();
    let names: Vec<_> = secrets
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["notes/database-url", "notes/s3-key"]);

    assert!(secrets.remove(&db).await.unwrap());
    assert!(!secrets.remove(&db).await.unwrap());
    assert!(secrets.reveal(db.as_ref()).await.unwrap().is_none());
}

#[tokio::test]
async fn values_are_encrypted_at_rest_and_bound_to_their_name() {
    let dir = TempDir::new().unwrap();
    let db = store(&dir).await;
    let key = SecretKey::generate();
    let secrets = SecretStore::new(db.clone(), &key);
    secrets
        .set(&name("notes/a"), &"plaintext-value".into())
        .await
        .unwrap();
    let sealed = db.sealed_secret("notes/a").await.unwrap().unwrap();
    assert_eq!(sealed.nonce.len(), 24);
    assert!(
        !sealed
            .ciphertext
            .windows(b"plaintext-value".len())
            .any(|w| w == b"plaintext-value")
    );

    // The same sealed row under another name fails to open (AAD = name).
    db.put_secret(&SealedSecret {
        name: "notes/b".into(),
        ..sealed.clone()
    })
    .await
    .unwrap();
    assert!(matches!(
        secrets.reveal("notes/b").await,
        Err(SecretError::Decrypt(n)) if n == "notes/b"
    ));

    // Another key cannot open it either.
    let other = SecretStore::new(db, &SecretKey::generate());
    assert!(matches!(
        other.reveal("notes/a").await,
        Err(SecretError::Decrypt(_))
    ));
}

#[test]
fn key_file_is_generated_once_with_mode_0600() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("cfg/hoster/secret.key");
    let none = Credentials::default();
    assert!(SecretKey::load(&file, &none).unwrap().is_none());
    let key = SecretKey::generate_into(&file).unwrap();
    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let parent = fs::metadata(file.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(parent, 0o700);
    assert!(!file.with_file_name("secret.key.tmp").exists());
    // The second start reads the same key.
    assert_eq!(SecretKey::load(&file, &none).unwrap(), Some(key));
    // Never over an existing file.
    assert!(matches!(
        SecretKey::generate_into(&file),
        Err(SecretError::WriteKey { .. })
    ));
}

#[test]
fn key_file_readable_by_others_is_refused() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("secret.key");
    SecretKey::generate_into(&file).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        SecretKey::load(&file, &Credentials::default()),
        Err(SecretError::KeyPermissions { mode: 0o644, .. })
    ));
}

#[test]
fn invalid_key_file_is_refused() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("secret.key");
    fs::write(&file, "not base64!\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        SecretKey::load(&file, &Credentials::default()),
        Err(SecretError::InvalidKey { .. })
    ));
    fs::write(&file, format!("{}\n", BASE64.encode([1u8; 16]))).unwrap();
    assert!(matches!(
        SecretKey::load(&file, &Credentials::default()),
        Err(SecretError::InvalidKey { .. })
    ));
}

#[test]
fn systemd_credential_takes_precedence_over_the_file() {
    let dir = TempDir::new().unwrap();
    let creds = dir.path().join("creds");
    fs::create_dir(&creds).unwrap();
    let key = SecretKey([7; KEY_LEN]);
    fs::write(
        creds.join(KEY_CREDENTIAL),
        format!("{}\n", BASE64.encode(key.0)),
    )
    .unwrap();
    let file = dir.path().join("secret.key");
    let loaded = SecretKey::load(&file, &Credentials::in_dir(&creds)).unwrap();
    assert_eq!(loaded, Some(key));
    assert!(
        !file.exists(),
        "no key file is generated when the credential is set"
    );
}

#[tokio::test]
async fn open_generates_the_key_on_first_start_only() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("cfg/hoster/secret.key");
    let none = Credentials::default();
    let store = store(&dir).await;

    let secrets = SecretStore::open(store.clone(), &file, &none)
        .await
        .unwrap();
    assert!(file.exists());
    secrets.set(&name("notes/a"), &"x".into()).await.unwrap();

    // The second start opens the stored secrets with the same key.
    let again = SecretStore::open(store.clone(), &file, &none)
        .await
        .unwrap();
    assert_eq!(
        again.reveal("notes/a").await.unwrap().unwrap().0.expose(),
        "x"
    );

    // A lost key file is not silently replaced while secrets are stored under the old key.
    fs::remove_file(&file).unwrap();
    assert!(matches!(
        SecretStore::open(store, &file, &none).await,
        Err(SecretError::KeyMissing { stored: 1, .. })
    ));
    assert!(!file.exists());
}
