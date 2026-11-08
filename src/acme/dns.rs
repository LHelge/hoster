//! The advisory DNS check before issuance (ADR-0036, ADR-0051 #2).
//!
//! [`DnsCheck`] compares a host's A/AAAA records with hoster's public addresses: the admin
//! host's records, re-resolved by [`DnsCheck::refresh`], or `acme.public_addresses` when
//! configured. Clones share the resolved addresses, so the ACME task (which refreshes them
//! hourly) and the certificates page of the web UI (which shows them and re-runs the check on
//! demand) see the same picture.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::config::Config;

/// A/AAAA lookups for the advisory DNS check.
#[async_trait]
pub trait Resolve: Send + Sync {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>>;
}

/// The system resolver (`getaddrinfo`, through tokio).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

#[async_trait]
impl Resolve for SystemResolver {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        let addrs = tokio::net::lookup_host((host, 0)).await?;
        Ok(addrs.map(|addr| addr.ip()).collect())
    }
}

/// Where hoster's public addresses for the DNS check come from (ADR-0051 #2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicAddresses {
    /// `acme.public_addresses`.
    Configured(BTreeSet<IpAddr>),
    /// The A/AAAA records of this host (the admin host), re-resolved on every refresh.
    OfHost(String),
}

impl From<&Config> for PublicAddresses {
    fn from(config: &Config) -> Self {
        if config.acme.public_addresses.is_empty() {
            PublicAddresses::OfHost(config.admin_host.to_string())
        } else {
            PublicAddresses::Configured(config.acme.public_addresses.iter().copied().collect())
        }
    }
}

impl fmt::Display for PublicAddresses {
    /// Where the addresses come from, as the certificates page says it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublicAddresses::Configured(_) => f.write_str("acme.public_addresses in hoster.toml"),
            PublicAddresses::OfHost(host) => write!(f, "the A/AAAA records of {host}"),
        }
    }
}

/// A host whose A/AAAA records are not (all) hoster's public addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsMismatch {
    pub host: String,
    /// The host's addresses, sorted; empty when it does not resolve.
    pub observed: Vec<IpAddr>,
    /// hoster's public addresses, sorted.
    pub expected: Vec<IpAddr>,
}

/// The outcome of one comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsVerdict {
    /// Every address of the host is one of hoster's.
    Matches,
    /// The host resolves elsewhere, or not at all.
    Mismatch(DnsMismatch),
    /// hoster's own public addresses are not known, so there is nothing to compare with.
    Unknown,
}

/// The advisory DNS check, sharing hoster's resolved public addresses between clones.
#[derive(Clone)]
pub struct DnsCheck {
    resolver: Arc<dyn Resolve>,
    public: Arc<PublicAddresses>,
    /// hoster's public addresses as last resolved; empty until they resolve once.
    expected: Arc<Mutex<BTreeSet<IpAddr>>>,
}

impl fmt::Debug for DnsCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DnsCheck")
            .field("public", &self.public)
            .field("expected", &self.expected())
            .finish_non_exhaustive()
    }
}

impl From<PublicAddresses> for DnsCheck {
    /// The check through the system resolver.
    fn from(public: PublicAddresses) -> Self {
        Self::new(public, Arc::new(SystemResolver))
    }
}

impl DnsCheck {
    /// The check for `public`, resolving names through `resolver`. Configured addresses are
    /// known right away; a host's are known after the first [`refresh`](Self::refresh).
    pub fn new(public: PublicAddresses, resolver: Arc<dyn Resolve>) -> Self {
        let expected = match &public {
            PublicAddresses::Configured(addresses) => addresses.clone(),
            PublicAddresses::OfHost(_) => BTreeSet::new(),
        };
        Self {
            resolver,
            public: Arc::new(public),
            expected: Arc::new(Mutex::new(expected)),
        }
    }

    /// The same source of public addresses, resolved through `resolver` (and no longer shared
    /// with `self`'s clones).
    pub fn with_resolver(&self, resolver: Arc<dyn Resolve>) -> Self {
        Self::new(PublicAddresses::clone(&self.public), resolver)
    }

    /// Where hoster's public addresses come from.
    pub fn source(&self) -> &PublicAddresses {
        &self.public
    }

    /// hoster's public addresses as last resolved, sorted; empty when they are not known.
    pub fn expected(&self) -> BTreeSet<IpAddr> {
        self.expected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_expected(&self, addresses: BTreeSet<IpAddr>) {
        *self
            .expected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = addresses;
    }

    /// Re-resolves hoster's public addresses, keeping the previous ones on a failure.
    pub async fn refresh(&self) {
        let PublicAddresses::OfHost(host) = self.public.as_ref() else {
            return;
        };
        match self.resolver.lookup(host).await {
            Ok(addresses) if !addresses.is_empty() => {
                let addresses: BTreeSet<IpAddr> = addresses.into_iter().collect();
                if addresses != self.expected() {
                    tracing::info!(host, ?addresses, "resolved hoster's public addresses");
                }
                self.set_expected(addresses);
            }
            Ok(_) => tracing::warn!(host, "the admin host has no A/AAAA records"),
            Err(error) => {
                tracing::warn!(host, %error, "cannot resolve the admin host's addresses");
            }
        }
    }

    /// Compares `host`'s records with hoster's public addresses, resolving those first if they
    /// are not known yet.
    pub async fn check(&self, host: &str) -> DnsVerdict {
        if self.expected().is_empty() {
            self.refresh().await;
        }
        let expected = self.expected();
        if expected.is_empty() {
            return DnsVerdict::Unknown;
        }
        let observed: BTreeSet<IpAddr> = match self.resolver.lookup(host).await {
            Ok(addresses) => addresses.into_iter().collect(),
            Err(error) => {
                tracing::debug!(host, %error, "cannot resolve host");
                BTreeSet::new()
            }
        };
        if !observed.is_empty() && observed.is_subset(&expected) {
            return DnsVerdict::Matches;
        }
        DnsVerdict::Mismatch(DnsMismatch {
            host: host.to_owned(),
            observed: observed.into_iter().collect(),
            expected: expected.into_iter().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct Records(HashMap<&'static str, Vec<IpAddr>>);

    #[async_trait]
    impl Resolve for Records {
        async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
            self.0
                .get(host)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "NXDOMAIN"))
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn check() -> DnsCheck {
        let records = Records(HashMap::from([
            (
                "admin.example.com",
                vec![ip("192.0.2.1"), ip("2001:db8::1")],
            ),
            ("ok.example.com", vec![ip("192.0.2.1")]),
            ("cdn.example.com", vec![ip("203.0.113.9"), ip("192.0.2.1")]),
        ]));
        DnsCheck::new(
            PublicAddresses::OfHost("admin.example.com".into()),
            Arc::new(records),
        )
    }

    #[tokio::test]
    async fn hosts_are_compared_with_the_admin_hosts_records() {
        let dns = check();
        assert!(dns.expected().is_empty(), "nothing resolved yet");
        assert_eq!(dns.check("ok.example.com").await, DnsVerdict::Matches);
        assert_eq!(
            dns.expected(),
            BTreeSet::from([ip("192.0.2.1"), ip("2001:db8::1")])
        );
        assert_eq!(
            dns.check("cdn.example.com").await,
            DnsVerdict::Mismatch(DnsMismatch {
                host: "cdn.example.com".into(),
                observed: vec![ip("192.0.2.1"), ip("203.0.113.9")],
                expected: vec![ip("192.0.2.1"), ip("2001:db8::1")],
            })
        );
        let DnsVerdict::Mismatch(missing) = dns.check("typo.example.com").await else {
            panic!("an unresolvable host does not match");
        };
        assert!(missing.observed.is_empty());
    }

    #[tokio::test]
    async fn clones_share_the_resolved_addresses() {
        let dns = check();
        let page = dns.clone();
        dns.refresh().await;
        assert_eq!(page.expected().len(), 2);
    }

    #[tokio::test]
    async fn without_public_addresses_nothing_is_compared() {
        let dns = DnsCheck::new(
            PublicAddresses::OfHost("unresolvable.example.com".into()),
            Arc::new(Records(HashMap::new())),
        );
        assert_eq!(dns.check("ok.example.com").await, DnsVerdict::Unknown);

        let configured = DnsCheck::new(
            PublicAddresses::Configured(BTreeSet::from([ip("203.0.113.9")])),
            Arc::new(Records(HashMap::new())),
        );
        assert_eq!(configured.expected().len(), 1);
        assert_eq!(
            configured.source().to_string(),
            "acme.public_addresses in hoster.toml"
        );
    }
}
