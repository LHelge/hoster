use std::os::unix::fs::PermissionsExt;

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, date_time_ymd};

use super::*;

/// A CA-signed leaf for `hosts`, as (leaf PEM, CA PEM, leaf key PEM).
struct Issued {
    leaf: String,
    ca: String,
    key: String,
}

impl Issued {
    fn new(hosts: &[&str]) -> Self {
        Self::with(hosts, |_| {})
    }

    fn with(hosts: &[&str], tweak: impl FnOnce(&mut CertificateParams)) -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name.push(DnType::CommonName, "Test CA");
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca, ca_key);

        let key = KeyPair::generate().unwrap();
        let mut params =
            CertificateParams::new(hosts.iter().map(|h| h.to_string()).collect::<Vec<_>>())
                .unwrap();
        params.distinguished_name.push(DnType::CommonName, hosts[0]);
        params.use_authority_key_identifier_extension = true;
        tweak(&mut params);
        let leaf = params.signed_by(&key, &issuer).unwrap();
        Self {
            leaf: leaf.pem(),
            ca: ca_cert.pem(),
            key: key.serialize_pem(),
        }
    }

    fn fullchain(&self) -> String {
        format!("{}{}", self.leaf, self.ca)
    }
}

fn open(dir: &Path) -> (CertMap, LoadReport) {
    CertMap::open(dir, Some("https://ca.example/dir".to_owned())).unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn leaf_der(map: &CertMap, host: &str) -> Vec<u8> {
    map.get(host).unwrap().end_entity_cert().unwrap().to_vec()
}

#[test]
fn insert_writes_the_layout_atomically_with_private_key_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let (map, report) = open(dir.path());
    assert!(report.loaded.is_empty());

    let cert = Issued::new(&["app.example.com"]);
    let validity = map
        .insert("App.Example.com", cert.fullchain(), cert.key.clone())
        .unwrap();

    let host_dir = dir.path().join("certs/app.example.com");
    assert_eq!(
        fs::read_to_string(host_dir.join("fullchain.pem")).unwrap(),
        cert.fullchain()
    );
    assert_eq!(
        fs::read_to_string(host_dir.join("privkey.pem")).unwrap(),
        cert.key
    );
    assert_eq!(mode(&host_dir.join("privkey.pem")), 0o600);
    assert_eq!(mode(&host_dir), 0o700);
    assert_eq!(mode(&dir.path().join("certs")), 0o700);
    for leftover in ["fullchain.pem.tmp", "privkey.pem.tmp", "meta.json.tmp"] {
        assert!(!host_dir.join(leftover).exists(), "{leftover} left behind");
    }

    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(host_dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["directory"], "https://ca.example/dir");
    assert!(meta["ari_cert_id"].as_str().unwrap().contains('.'));
    let not_after: SystemTime = humantime::parse_rfc3339(meta["not_after"].as_str().unwrap())
        .expect("not_after is RFC 3339");
    assert_eq!(not_after, validity.not_after);
    assert!(meta["issued_at"].is_string());

    assert_eq!(map.hosts(), ["app.example.com"]);
    assert_eq!(map.validity("app.example.com"), Some(validity));
    assert!(map.get("APP.example.com").is_some());
}

#[test]
fn certified_key_holds_the_leaf_first_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let cert = Issued::new(&["app.example.com"]);
    map.insert("app.example.com", cert.fullchain(), cert.key.clone())
        .unwrap();
    let key = map.get("app.example.com").unwrap();
    assert_eq!(key.cert.len(), 2);
    key.keys_match().expect("leaf matches the key");
}

#[test]
fn chain_in_reverse_order_is_reordered_leaf_first() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let cert = Issued::new(&["app.example.com"]);
    let reversed = format!("{}{}", cert.ca, cert.leaf);
    map.insert("app.example.com", reversed.clone(), cert.key.clone())
        .unwrap();

    let key = map.get("app.example.com").unwrap();
    let leaf = CertificateDer::from_pem_slice(cert.leaf.as_bytes()).unwrap();
    let ca = CertificateDer::from_pem_slice(cert.ca.as_bytes()).unwrap();
    assert_eq!(key.cert, vec![leaf, ca]);
    // The file keeps what the CA sent; loading reorders it again.
    let (reloaded, report) = open(dir.path());
    assert_eq!(report.loaded, ["app.example.com"]);
    assert_eq!(reloaded.get("app.example.com").unwrap().cert, key.cert);
}

#[test]
fn mismatched_key_is_rejected_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let cert = Issued::new(&["app.example.com"]);
    let other = Issued::new(&["app.example.com"]);
    let error = map
        .insert("app.example.com", cert.fullchain(), other.key)
        .unwrap_err();
    assert!(matches!(error, CertError::KeyMismatch { .. }), "{error}");
    assert!(map.get("app.example.com").is_none());
    assert!(!dir.path().join("certs/app.example.com").exists());
}

#[test]
fn certificate_for_another_host_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let cert = Issued::new(&["other.example.com"]);
    let error = map
        .insert("app.example.com", cert.fullchain(), cert.key)
        .unwrap_err();
    assert!(matches!(error, CertError::NameMismatch { .. }), "{error}");
}

#[test]
fn wildcard_san_covers_one_label() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let cert = Issued::new(&["*.example.com"]);
    map.insert("app.example.com", cert.fullchain(), cert.key.clone())
        .unwrap();
    let error = map
        .insert("a.b.example.com", cert.fullchain(), cert.key)
        .unwrap_err();
    assert!(matches!(error, CertError::NameMismatch { .. }), "{error}");
}

#[test]
fn garbage_and_empty_input_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let cert = Issued::new(&["app.example.com"]);
    assert!(matches!(
        map.insert("app.example.com", "", cert.key.clone()),
        Err(CertError::EmptyChain { .. })
    ));
    assert!(matches!(
        map.insert("app.example.com", cert.fullchain(), "not a key"),
        Err(CertError::KeyPem { .. })
    ));
}

#[test]
fn unsafe_hostnames_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let cert = Issued::new(&["app.example.com"]);
    for host in [
        "",
        "..",
        "../etc",
        "a/b",
        "a..b",
        ".a",
        "a.",
        "a_b.example.com",
    ] {
        let error = map
            .insert(host, cert.fullchain(), cert.key.clone())
            .unwrap_err();
        assert!(
            matches!(error, CertError::InvalidHost { .. }),
            "{host}: {error}"
        );
    }
    assert!(!dir.path().join("certs").exists());
}

#[test]
fn expired_certificate_is_rejected_at_insert() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let cert = Issued::with(&["old.example.com"], |p| {
        p.not_before = date_time_ymd(2020, 1, 1);
        p.not_after = date_time_ymd(2020, 4, 1);
    });
    let error = map
        .insert("old.example.com", cert.fullchain(), cert.key)
        .unwrap_err();
    assert!(matches!(error, CertError::Expired { .. }), "{error}");
}

#[test]
fn open_loads_valid_certificates_and_flags_expired_and_broken_ones() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (map, _) = open(dir.path());
        let a = Issued::new(&["a.example.com"]);
        let b = Issued::new(&["b.example.com"]);
        map.insert("a.example.com", a.fullchain(), a.key).unwrap();
        map.insert("b.example.com", b.fullchain(), b.key).unwrap();
    }
    // An expired certificate written by an earlier run.
    let certs = dir.path().join("certs");
    let old = Issued::with(&["old.example.com"], |p| {
        p.not_before = date_time_ymd(2020, 1, 1);
        p.not_after = date_time_ymd(2020, 4, 1);
    });
    fs::create_dir_all(certs.join("old.example.com")).unwrap();
    fs::write(certs.join("old.example.com/fullchain.pem"), old.fullchain()).unwrap();
    fs::write(certs.join("old.example.com/privkey.pem"), old.key).unwrap();
    // A half-written pair: new key next to an old chain.
    let torn = Issued::new(&["torn.example.com"]);
    let other = Issued::new(&["torn.example.com"]);
    fs::create_dir_all(certs.join("torn.example.com")).unwrap();
    fs::write(
        certs.join("torn.example.com/fullchain.pem"),
        torn.fullchain(),
    )
    .unwrap();
    fs::write(certs.join("torn.example.com/privkey.pem"), other.key).unwrap();
    // A directory with no files, and a stray file.
    fs::create_dir_all(certs.join("empty.example.com")).unwrap();
    fs::write(certs.join("README"), "not a host").unwrap();

    let (map, report) = open(dir.path());
    assert_eq!(report.loaded, ["a.example.com", "b.example.com"]);
    assert_eq!(report.expired, ["old.example.com"]);
    let invalid: Vec<&str> = report.invalid.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(invalid, ["empty.example.com", "torn.example.com"]);
    assert!(matches!(report.invalid[0].1, CertError::Read { .. }));
    assert!(matches!(report.invalid[1].1, CertError::KeyMismatch { .. }));

    assert_eq!(map.hosts(), ["a.example.com", "b.example.com"]);
    assert!(map.get("old.example.com").is_none());
}

#[test]
fn missing_certs_directory_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let (map, report) = open(dir.path());
    assert!(map.hosts().is_empty());
    assert!(report.loaded.is_empty() && report.expired.is_empty() && report.invalid.is_empty());
}

#[test]
fn insert_replaces_atomically_and_old_snapshots_stay_intact() {
    let dir = tempfile::tempdir().unwrap();
    let (map, _) = open(dir.path());
    let first = Issued::new(&["app.example.com"]);
    let second = Issued::new(&["app.example.com"]);
    let neighbour = Issued::new(&["other.example.com"]);
    map.insert("app.example.com", first.fullchain(), first.key)
        .unwrap();
    map.insert("other.example.com", neighbour.fullchain(), neighbour.key)
        .unwrap();

    let before = map.snapshot.load_full();
    let old_leaf = leaf_der(&map, "app.example.com");
    let clone = map.clone();
    clone
        .insert("app.example.com", second.fullchain(), second.key.clone())
        .unwrap();

    let new_leaf = leaf_der(&map, "app.example.com");
    assert_ne!(old_leaf, new_leaf, "every clone sees the replacement");
    assert_eq!(
        new_leaf,
        CertificateDer::from_pem_slice(second.leaf.as_bytes())
            .unwrap()
            .to_vec()
    );
    // A reader holding the previous snapshot still sees the previous certificate.
    assert_eq!(
        before["app.example.com"]
            .key
            .end_entity_cert()
            .unwrap()
            .to_vec(),
        old_leaf
    );
    assert!(map.get("other.example.com").is_some(), "other hosts kept");

    // And the replacement is what the next start loads.
    let (reloaded, _) = open(dir.path());
    assert_eq!(leaf_der(&reloaded, "app.example.com"), new_leaf);
}
