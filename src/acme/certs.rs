//! Certificates: validated on the way in, persisted under `certs/<host>/` (ADR-0016,
//! docs/architecture.md §7.4) and published to the proxy as an `ArcSwap`'d SNI map (ADR-0014,
//! §3.2).
//!
//! [`CertMap`] is the whole public surface. [`CertMap::open`] loads every certificate on disk at
//! startup (architecture §4 step 3), skipping and reporting expired or broken ones;
//! [`CertMap::insert`] is how the ACME task installs a freshly issued certificate: it validates
//! the chain against the key, writes the files atomically and publishes a new snapshot;
//! [`CertMap::get`] is the lock-free lookup the `CertResolver` does on every handshake.
//!
//! Layout, one directory per hostname:
//!
//! ```text
//! certs/<host>/fullchain.pem   leaf first, then intermediates (as the CA sent it)
//! certs/<host>/privkey.pem     mode 0600
//! certs/<host>/meta.json       not_before, not_after, issued_at, CA directory, ARI cert id
//! ```
//!
//! Each file is replaced atomically on its own. A crash between the renames can leave a new key
//! next to an old chain; such a pair fails the key check on the next start, is reported as
//! invalid and gets re-issued, so it is never served.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use instant_acme::CertificateIdentifier;
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use serde::Serialize;
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;

use super::atomic::AtomicFile;

/// Why a certificate was rejected or could not be stored.
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("{host:?} is not a valid certificate hostname")]
    InvalidHost { host: String },
    #[error("the certificate chain for {host} is not valid PEM")]
    ChainPem {
        host: String,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("the certificate chain for {host} is empty")]
    EmptyChain { host: String },
    #[error("the private key for {host} is not valid PEM")]
    KeyPem {
        host: String,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("the private key for {host} is not supported")]
    Key {
        host: String,
        #[source]
        source: rustls::Error,
    },
    #[error("a certificate for {host} is not valid X.509: {reason}")]
    X509 { host: String, reason: String },
    #[error("no certificate in the chain for {host} matches its private key")]
    KeyMismatch { host: String },
    #[error("the certificate for {host} does not name {host} in its subject alternative names")]
    NameMismatch { host: String },
    #[error("the certificate for {host} expired at {}", humantime::format_rfc3339_seconds(*.not_after))]
    Expired { host: String, not_after: SystemTime },
    #[error("cannot read {}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot write {}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// When a certificate is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    pub not_before: SystemTime,
    pub not_after: SystemTime,
}

impl Validity {
    /// Whether the certificate is past its end of validity at `now`.
    pub fn is_expired_at(&self, now: SystemTime) -> bool {
        now >= self.not_after
    }

    /// When the certificate is due for renewal: once less than a third of its lifetime remains
    /// (ADR-0015).
    pub fn renew_at(&self) -> SystemTime {
        let lifetime = self
            .not_after
            .duration_since(self.not_before)
            .unwrap_or_default();
        self.not_after - lifetime / 3
    }
}

impl From<&X509Certificate<'_>> for Validity {
    fn from(cert: &X509Certificate<'_>) -> Self {
        let time = |seconds: i64| match u64::try_from(seconds) {
            Ok(after) => SystemTime::UNIX_EPOCH + Duration::from_secs(after),
            Err(_) => SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs()),
        };
        Validity {
            not_before: time(cert.validity().not_before.timestamp()),
            not_after: time(cert.validity().not_after.timestamp()),
        }
    }
}

/// A host's certificate as the proxy serves it.
#[derive(Debug, Clone)]
pub struct CertEntry {
    /// The rustls key and leaf-first chain, built with the ring provider (ADR-0037).
    pub key: Arc<CertifiedKey>,
    pub validity: Validity,
}

/// The contents of `meta.json`.
#[derive(Debug, Clone, Serialize)]
struct CertMeta {
    #[serde(with = "humantime_serde")]
    not_before: SystemTime,
    #[serde(with = "humantime_serde")]
    not_after: SystemTime,
    #[serde(with = "humantime_serde")]
    issued_at: SystemTime,
    /// The ACME directory the certificate was issued by, if known.
    directory: Option<String>,
    /// The ACME renewal information (RFC 9773) certificate identifier, when the leaf has an
    /// authority key identifier.
    ari_cert_id: Option<String>,
}

/// A validated certificate chain and private key for one hostname. No `Debug` of the key PEM.
struct Certificate {
    host: String,
    chain_pem: Vec<u8>,
    key_pem: Vec<u8>,
    entry: CertEntry,
    ari_cert_id: Option<String>,
}

impl fmt::Debug for Certificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificate")
            .field("host", &self.host)
            .field("validity", &self.entry.validity)
            .finish_non_exhaustive()
    }
}

impl Certificate {
    /// Parses and checks `chain_pem` and `key_pem` for `host`: the key must be one ring can sign
    /// with, one certificate in the chain must carry its public key and name `host`, and the chain
    /// is put in leaf-first order whatever order the PEM had.
    fn new(host: &str, chain_pem: Vec<u8>, key_pem: Vec<u8>) -> Result<Self, CertError> {
        let host = Self::normalize_host(host)?;
        let chain = CertificateDer::pem_slice_iter(&chain_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| CertError::ChainPem {
                host: host.clone(),
                source,
            })?;
        if chain.is_empty() {
            return Err(CertError::EmptyChain { host });
        }
        let key_der =
            PrivateKeyDer::from_pem_slice(&key_pem).map_err(|source| CertError::KeyPem {
                host: host.clone(),
                source,
            })?;
        let signing_key = any_supported_type(&key_der).map_err(|source| CertError::Key {
            host: host.clone(),
            source,
        })?;
        let spki = signing_key
            .public_key()
            .ok_or_else(|| CertError::KeyMismatch { host: host.clone() })?;

        let (chain, validity) = {
            let parsed = chain
                .iter()
                .map(|der| {
                    x509_parser::parse_x509_certificate(der)
                        .map(|(_, cert)| cert)
                        .map_err(|e| CertError::X509 {
                            host: host.clone(),
                            reason: e.to_string(),
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let leaf = parsed
                .iter()
                .position(|cert| cert.public_key().raw == spki.as_ref())
                .ok_or_else(|| CertError::KeyMismatch { host: host.clone() })?;
            if !Self::names(&parsed[leaf], &host) {
                return Err(CertError::NameMismatch { host });
            }
            let validity = Validity::from(&parsed[leaf]);
            let order = Self::leaf_first_order(&parsed, leaf);
            drop(parsed);
            let mut slots: Vec<Option<CertificateDer<'static>>> =
                chain.into_iter().map(Some).collect();
            let chain = order
                .into_iter()
                .map(|i| slots[i].take().expect("each index appears once"))
                .collect::<Vec<_>>();
            (chain, validity)
        };

        let ari_cert_id = CertificateIdentifier::try_from(&chain[0])
            .ok()
            .map(|id| id.to_string());
        let key = CertifiedKey::new(chain, signing_key);
        key.keys_match()
            .map_err(|_| CertError::KeyMismatch { host: host.clone() })?;
        Ok(Self {
            host,
            chain_pem,
            key_pem,
            entry: CertEntry {
                key: Arc::new(key),
                validity,
            },
            ari_cert_id,
        })
    }

    /// `host` lowercased, if it is a plain DNS name that is also safe as a directory name.
    fn normalize_host(host: &str) -> Result<String, CertError> {
        let lower = host.to_ascii_lowercase();
        let valid = !lower.is_empty()
            && lower.len() <= 253
            && lower.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
        if valid {
            Ok(lower)
        } else {
            Err(CertError::InvalidHost {
                host: host.to_owned(),
            })
        }
    }

    /// Whether `cert`'s DNS subject alternative names cover `host` (exactly, or by a wildcard for
    /// its first label).
    fn names(cert: &X509Certificate<'_>, host: &str) -> bool {
        let Ok(Some(san)) = cert.subject_alternative_name() else {
            return false;
        };
        san.value.general_names.iter().any(|name| match name {
            GeneralName::DNSName(dns) => {
                let dns = dns.to_ascii_lowercase();
                match dns.strip_prefix("*.") {
                    Some(parent) => host.split_once('.').is_some_and(|(_, rest)| rest == parent),
                    None => dns == host,
                }
            }
            _ => false,
        })
    }

    /// Indices of `certs` starting at `leaf` and following issuer → subject links; certificates
    /// outside that path keep their relative order at the end.
    fn leaf_first_order(certs: &[X509Certificate<'_>], leaf: usize) -> Vec<usize> {
        let mut order = vec![leaf];
        let mut current = leaf;
        loop {
            let issuer = certs[current].issuer().as_raw();
            // A self-signed certificate is its own issuer: the path ends there.
            if certs[current].subject().as_raw() == issuer {
                break;
            }
            match (0..certs.len())
                .find(|i| !order.contains(i) && certs[*i].subject().as_raw() == issuer)
            {
                Some(next) => {
                    order.push(next);
                    current = next;
                }
                None => break,
            }
        }
        let rest: Vec<usize> = (0..certs.len()).filter(|i| !order.contains(i)).collect();
        order.extend(rest);
        order
    }
}

/// The `certs/` directory.
#[derive(Debug, Clone)]
struct CertStore {
    dir: PathBuf,
}

/// The outcome of loading the certificates on disk.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Hosts whose certificate is now served.
    pub loaded: Vec<String>,
    /// Hosts whose certificate on disk has expired; they need issuing again.
    pub expired: Vec<String>,
    /// Directories under `certs/` that do not hold a usable certificate, with the reason.
    pub invalid: Vec<(String, CertError)>,
}

impl CertStore {
    const FULLCHAIN: &str = "fullchain.pem";
    const PRIVKEY: &str = "privkey.pem";
    const META: &str = "meta.json";

    fn host_dir(&self, host: &str) -> PathBuf {
        self.dir.join(host)
    }

    /// Writes `cert`'s three files atomically, key first.
    fn save(&self, cert: &Certificate, meta: &CertMeta) -> Result<(), CertError> {
        let dir = self.host_dir(&cert.host);
        let meta = serde_json::to_vec_pretty(meta).expect("CertMeta serializes");
        for (name, contents, mode) in [
            (Self::PRIVKEY, cert.key_pem.as_slice(), AtomicFile::PRIVATE),
            (
                Self::FULLCHAIN,
                cert.chain_pem.as_slice(),
                AtomicFile::PUBLIC,
            ),
            (Self::META, meta.as_slice(), AtomicFile::PUBLIC),
        ] {
            let path = dir.join(name);
            AtomicFile::new(&path, mode)
                .write(contents)
                .map_err(|source| CertError::Write { path, source })?;
        }
        Ok(())
    }

    /// Reads and validates the certificate in `certs/<name>/`.
    fn read(&self, name: &str) -> Result<Certificate, CertError> {
        let dir = self.host_dir(name);
        let read = |file: &str| {
            let path = dir.join(file);
            fs::read(&path).map_err(|source| CertError::Read { path, source })
        };
        let chain = read(Self::FULLCHAIN)?;
        let key = read(Self::PRIVKEY)?;
        let cert = Certificate::new(name, chain, key)?;
        if cert.host != name {
            // Directories are always written lowercased; anything else was not written by hoster.
            return Err(CertError::InvalidHost {
                host: name.to_owned(),
            });
        }
        Ok(cert)
    }

    /// Every certificate on disk that is valid at `now`. A missing `certs/` directory is empty.
    fn load_all(&self, now: SystemTime) -> Result<(Vec<Certificate>, LoadReport), CertError> {
        let read_error = |source| CertError::Read {
            path: self.dir.clone(),
            source,
        };
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((Vec::new(), LoadReport::default()));
            }
            Err(source) => return Err(read_error(source)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(read_error)?;
            if entry.file_type().map_err(read_error)?.is_dir() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();

        let mut certs = Vec::new();
        let mut report = LoadReport::default();
        for name in names {
            match self.read(&name) {
                Ok(cert) if cert.entry.validity.is_expired_at(now) => report.expired.push(name),
                Ok(cert) => {
                    report.loaded.push(name);
                    certs.push(cert);
                }
                Err(error) => report.invalid.push((name, error)),
            }
        }
        Ok((certs, report))
    }
}

/// The SNI → certificate map the TLS listener reads (ADR-0014). Cloning shares the same map.
///
/// The ACME task (and the startup loader) is the only writer; every write builds a new snapshot
/// and stores it, so readers never see a partial update and never block.
#[derive(Debug, Clone)]
pub struct CertMap {
    store: CertStore,
    /// The ACME directory recorded in `meta.json` of certificates inserted from now on.
    directory: Option<String>,
    snapshot: Arc<ArcSwap<HashMap<String, CertEntry>>>,
    /// Serializes inserts across clones, so two inserts for one host cannot interleave their
    /// file writes (they share the `.tmp` names) or leave disk and snapshot disagreeing.
    writer: Arc<Mutex<()>>,
}

impl CertMap {
    /// The `certs/` directory relative to the state directory.
    const RELATIVE_PATH: &str = "certs";

    /// Loads the certificates under `state_dir/certs` that are valid now. Certificates inserted
    /// later record `directory` as the CA that issued them.
    ///
    /// Only an unreadable `certs/` directory is an error; expired and broken certificates are
    /// skipped and listed in the report.
    pub fn open(
        state_dir: &Path,
        directory: Option<String>,
    ) -> Result<(Self, LoadReport), CertError> {
        let store = CertStore {
            dir: state_dir.join(Self::RELATIVE_PATH),
        };
        let (certs, report) = store.load_all(SystemTime::now())?;
        let map = certs
            .into_iter()
            .map(|cert| (cert.host, cert.entry))
            .collect();
        Ok((
            Self {
                store,
                directory,
                snapshot: Arc::new(ArcSwap::from_pointee(map)),
                writer: Arc::default(),
            },
            report,
        ))
    }

    /// `host` lowercased, if it is a name [`CertMap::insert`] accepts: a plain DNS name that is
    /// also safe as a directory name.
    pub fn normalize_host(host: &str) -> Result<String, CertError> {
        Certificate::normalize_host(host)
    }

    /// Validates `chain_pem` (PEM, any order) against `key_pem` for `host`, persists both under
    /// `certs/<host>/` and publishes a snapshot serving the new certificate, replacing any
    /// previous one for that host. A rejected certificate changes nothing.
    ///
    /// Blocking file I/O on a few small files.
    pub fn insert(
        &self,
        host: &str,
        chain_pem: impl Into<Vec<u8>>,
        key_pem: impl Into<Vec<u8>>,
    ) -> Result<Validity, CertError> {
        let cert = Certificate::new(host, chain_pem.into(), key_pem.into())?;
        let validity = cert.entry.validity;
        let now = SystemTime::now();
        if validity.is_expired_at(now) {
            return Err(CertError::Expired {
                host: cert.host,
                not_after: validity.not_after,
            });
        }
        let meta = CertMeta {
            not_before: validity.not_before,
            not_after: validity.not_after,
            issued_at: now,
            directory: self.directory.clone(),
            ari_cert_id: cert.ari_cert_id.clone(),
        };
        // The guard protects no data, so a panic in another insert leaves nothing to recover.
        let _writing = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.store.save(&cert, &meta)?;
        let Certificate { host, entry, .. } = cert;
        self.snapshot.rcu(|map| {
            let mut map = HashMap::clone(map);
            map.insert(host.clone(), entry.clone());
            map
        });
        tracing::info!(
            host,
            not_after = %humantime::format_rfc3339_seconds(validity.not_after),
            "certificate installed"
        );
        Ok(validity)
    }

    /// The certified key for the SNI name `server_name` (any case), if hoster has one.
    pub fn get(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        let snapshot = self.snapshot.load();
        match snapshot.get(server_name) {
            Some(entry) => Some(Arc::clone(&entry.key)),
            None => snapshot
                .get(&server_name.to_ascii_lowercase())
                .map(|entry| Arc::clone(&entry.key)),
        }
    }

    /// When `host`'s current certificate is valid, if it has one.
    pub fn validity(&self, host: &str) -> Option<Validity> {
        self.snapshot
            .load()
            .get(&host.to_ascii_lowercase())
            .map(|entry| entry.validity)
    }

    /// The hosts that currently have a certificate, sorted.
    #[allow(dead_code, reason = "for the certificates page of the web UI")]
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self.snapshot.load().keys().cloned().collect();
        hosts.sort();
        hosts
    }
}

#[cfg(test)]
#[path = "certs_tests.rs"]
mod tests;
