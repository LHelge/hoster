//! Certificates of the routed hosts, as the orchestrator sees them (ADR-0036, architecture §3.3
//! #1–#2).
//!
//! [`CertHosts`] lives under the orchestrator's lock. It knows the hosts each stack routes and
//! tells the ACME task about every change: an apply sends `Ensure` for the stack's hosts before
//! anything is deployed, and a host that no stack routes any more is `Forget`-ten, so its
//! certificate is not renewed. Commands are sent on an unbounded channel *inside* the critical
//! section, so the ACME task sees them in the order the changes happened and an apply never
//! waits for an issuance in flight.
//!
//! The ACME task's [`AcmeEvent`]s come back through [`Orchestrator::run_acme_events`], which
//! records them as each host's [`CertStatus`] and relays them on `ui_events`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::mpsc;

use super::{Orchestrator, UiEvent};
use crate::acme::{AcmeCommand, AcmeEvent, DnsMismatch, DnsVerdict};
use crate::runtime::ContainerRuntime;
use crate::stack::Stack;

/// The certificate state of one host, from the ACME task's events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CertStatus {
    /// The end of validity of the certificate issued last, if one was issued since startup.
    pub not_after: Option<SystemTime>,
    /// Why the last issuance failed, until one succeeds.
    pub error: Option<String>,
    /// When the ACME task tries again after `error`.
    pub retry_at: Option<SystemTime>,
    /// The host's DNS did not point at hoster at the last attempt (ADR-0051), until an attempt
    /// finds it pointing here.
    pub dns_mismatch: Option<DnsMismatch>,
    /// Whether DNS was compared since startup, by an attempt or from the certificates page; when
    /// it was and `dns_mismatch` is `None`, DNS pointed here.
    pub dns_checked: bool,
    /// `dns_mismatch` was reported for the attempt in flight. The ACME task reports a mismatch
    /// before each attempt it finds one for and nothing when DNS matches, so an outcome without
    /// one means DNS pointed here that time.
    mismatch_this_attempt: bool,
}

impl CertStatus {
    /// Takes `event` into account.
    fn record(&mut self, event: &AcmeEvent) {
        match event {
            AcmeEvent::Issued { not_after, .. } => {
                // A host behind a CDN is issued despite a mismatch (ADR-0051 #2); the mismatch
                // reported for this attempt still describes its DNS.
                let dns_mismatch = if self.mismatch_this_attempt {
                    self.dns_mismatch.take()
                } else {
                    None
                };
                *self = CertStatus {
                    not_after: Some(*not_after),
                    dns_mismatch,
                    dns_checked: true,
                    ..CertStatus::default()
                };
            }
            AcmeEvent::Failed {
                error, retry_at, ..
            } => {
                self.error = Some(error.clone());
                self.retry_at = Some(*retry_at);
                self.dns_checked = true;
                if !std::mem::take(&mut self.mismatch_this_attempt) {
                    self.dns_mismatch = None;
                }
            }
            AcmeEvent::DnsMismatch(mismatch) => {
                self.dns_mismatch = Some(mismatch.clone());
                self.dns_checked = true;
                self.mismatch_this_attempt = true;
            }
        }
    }

    /// Takes a comparison made outside an attempt into account (the certificates page's
    /// "Check DNS now"). An attempt in flight still reports its own.
    fn dns_checked(&mut self, verdict: &DnsVerdict) {
        match verdict {
            DnsVerdict::Matches => self.dns_mismatch = None,
            DnsVerdict::Mismatch(mismatch) => self.dns_mismatch = Some(mismatch.clone()),
            DnsVerdict::Unknown => return,
        }
        self.dns_checked = true;
    }
}

/// The routed hosts of every stack and the channel to the ACME task.
#[derive(Debug, Default)]
pub(super) struct CertHosts {
    acme: Option<mpsc::UnboundedSender<AcmeCommand>>,
    /// The hosts of each stack's latest applied definition.
    by_stack: BTreeMap<String, BTreeSet<String>>,
    status: BTreeMap<String, CertStatus>,
    /// Hosts sent in a `Forget` and not ensured since. Events the ACME task sent about them
    /// before it saw the `Forget` are dropped, so they do not reappear in the status.
    forgotten: BTreeSet<String>,
}

impl CertHosts {
    /// The hosts `definition` routes.
    fn of(definition: &Stack) -> BTreeSet<String> {
        definition
            .services
            .values()
            .flat_map(|service| &service.routes)
            .map(|route| route.host.to_string())
            .collect()
    }

    /// Every host some stack routes.
    fn all(&self) -> BTreeSet<String> {
        self.by_stack.values().flatten().cloned().collect()
    }

    /// Sends commands to the ACME task from now on, starting with every host known so far.
    pub(super) fn connect(&mut self, acme: mpsc::UnboundedSender<AcmeCommand>) {
        self.acme = Some(acme);
        self.ensure(self.all());
    }

    /// Replaces the whole picture (startup) and ensures every host.
    pub(super) fn restore<'a>(&mut self, definitions: impl IntoIterator<Item = &'a Stack>) {
        let before = self.all();
        self.by_stack = definitions
            .into_iter()
            .map(|d| (d.name.to_string(), Self::of(d)))
            .collect();
        self.ensure(self.all());
        self.forget_unrouted(before);
    }

    /// `definition` was applied: ensures its hosts and forgets the ones it no longer routes.
    pub(super) fn apply(&mut self, definition: &Stack) {
        let hosts = Self::of(definition);
        let before = self
            .by_stack
            .insert(definition.name.to_string(), hosts.clone())
            .unwrap_or_default();
        self.ensure(hosts);
        self.forget_unrouted(before);
    }

    /// `stack` was removed: forgets the hosts no other stack routes.
    pub(super) fn remove(&mut self, stack: &str) {
        if let Some(before) = self.by_stack.remove(stack) {
            self.forget_unrouted(before);
        }
    }

    /// Records an event of the ACME task.
    pub(super) fn record(&mut self, event: &AcmeEvent) {
        if self.forgotten.contains(event.host()) {
            return;
        }
        self.status
            .entry(event.host().to_owned())
            .or_default()
            .record(event);
    }

    /// Records a DNS comparison for `host` made outside an attempt. Ignored for hosts that are
    /// neither routed nor the admin host.
    pub(super) fn dns_checked(
        &mut self,
        host: &str,
        admin_host: Option<&str>,
        verdict: &DnsVerdict,
    ) {
        let host = host.to_ascii_lowercase();
        let known = self.status.contains_key(&host)
            || admin_host.is_some_and(|admin| admin.eq_ignore_ascii_case(&host))
            || self.by_stack.values().any(|hosts| hosts.contains(&host));
        if known && !self.forgotten.contains(&host) {
            self.status.entry(host).or_default().dns_checked(verdict);
        }
    }

    /// Asks the ACME task to try `host` again now; whether a running task was told.
    pub(super) fn retry(&self, host: &str) -> bool {
        self.acme.as_ref().is_some_and(|acme| {
            acme.send(AcmeCommand::Retry {
                host: host.to_ascii_lowercase(),
            })
            .is_ok()
        })
    }

    /// The status of every routed host, and of any other host the ACME task reported on (the
    /// admin host).
    pub(super) fn status(&self) -> BTreeMap<String, CertStatus> {
        let mut status = self.status.clone();
        for host in self.all() {
            status.entry(host).or_default();
        }
        status
    }

    fn ensure(&mut self, hosts: BTreeSet<String>) {
        for host in &hosts {
            self.forgotten.remove(host);
        }
        if !hosts.is_empty() {
            self.send(AcmeCommand::Ensure {
                hosts: hosts.into_iter().collect(),
            });
        }
    }

    /// Forgets each of `candidates` that no stack routes now.
    fn forget_unrouted(&mut self, candidates: BTreeSet<String>) {
        let routed = self.all();
        for host in candidates.difference(&routed) {
            self.status.remove(host);
            self.forgotten.insert(host.clone());
            self.send(AcmeCommand::Forget { host: host.clone() });
        }
    }

    fn send(&self, command: AcmeCommand) {
        if let Some(acme) = &self.acme
            && acme.send(command).is_err()
        {
            tracing::warn!("the ACME task has stopped; certificates are not managed");
        }
    }
}

impl<R: ContainerRuntime> Orchestrator<R> {
    /// Sends `AcmeCommand`s to the ACME task on `acme` from now on (architecture §3.3 #1),
    /// beginning with the hosts already known. Commands wait in the channel until the task
    /// starts.
    pub fn connect_acme(&self, acme: mpsc::UnboundedSender<AcmeCommand>) {
        self.lock().certs.connect(acme);
    }

    /// Records the ACME task's events and relays them to open UI pages as
    /// [`UiEvent::Certificate`], until the task drops its sender.
    pub async fn run_acme_events(self: Arc<Self>, mut events: mpsc::Receiver<AcmeEvent>) {
        while let Some(event) = events.recv().await {
            self.lock().certs.record(&event);
            self.emit(UiEvent::Certificate(event));
        }
    }

    /// The certificate status of every routed host (and of the admin host once the ACME task
    /// reported on it), by host.
    pub fn certificates(&self) -> BTreeMap<String, CertStatus> {
        self.lock().certs.status()
    }

    /// Records the outcome of a DNS comparison for `host` made from the certificates page, so
    /// the status shows it until the next attempt reports its own. `admin_host` is recorded
    /// too, even before the ACME task reported on it.
    pub fn record_dns_check(&self, host: &str, admin_host: &str, verdict: &DnsVerdict) {
        self.lock()
            .certs
            .dns_checked(host, Some(admin_host), verdict);
    }

    /// Asks the ACME task to attempt `host` now (`AcmeCommand::Retry`). The task skips hosts it
    /// does not manage, hosts still backing off and certificates that are not due. `false` when
    /// no ACME task is connected or it has stopped.
    pub fn retry_certificate(&self, host: &str) -> bool {
        self.lock().certs.retry(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack(name: &str, hosts: &[&str]) -> Stack {
        let mut toml = format!("version = 1\nname = \"{name}\"\n");
        for (i, host) in hosts.iter().enumerate() {
            toml.push_str(&format!(
                "\n[services.s{i}]\nimage = \"ghcr.io/acme/s{i}\"\nroutes = [{{ host = \"{host}\", port = 80 }}]\n"
            ));
        }
        if hosts.is_empty() {
            toml.push_str("\n[services.worker]\nimage = \"ghcr.io/acme/worker\"\n");
        }
        Stack::parse(&toml).expect("valid stack").stack
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<AcmeCommand>) -> Vec<AcmeCommand> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn ensure(hosts: &[&str]) -> AcmeCommand {
        AcmeCommand::Ensure {
            hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
        }
    }

    fn forget(host: &str) -> AcmeCommand {
        AcmeCommand::Forget {
            host: host.to_owned(),
        }
    }

    #[test]
    fn applies_ensure_their_hosts_and_forget_the_ones_no_stack_routes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut certs = CertHosts::default();
        certs.restore([&stack("blog", &["blog.example.com"])]);
        certs.connect(tx);
        assert_eq!(drain(&mut rx), [ensure(&["blog.example.com"])]);

        certs.apply(&stack("notes", &["notes.example.com", "api.example.com"]));
        assert_eq!(
            drain(&mut rx),
            [ensure(&["api.example.com", "notes.example.com"])]
        );

        // notes drops api and takes over blog's host too; api is forgotten, blog is not.
        certs.apply(&stack("notes", &["notes.example.com", "blog.example.com"]));
        assert_eq!(
            drain(&mut rx),
            [
                ensure(&["blog.example.com", "notes.example.com"]),
                forget("api.example.com")
            ]
        );

        certs.remove("blog");
        assert_eq!(drain(&mut rx), []);
        certs.remove("notes");
        assert_eq!(
            drain(&mut rx),
            [forget("blog.example.com"), forget("notes.example.com")]
        );
        certs.apply(&stack("jobs", &[]));
        assert_eq!(drain(&mut rx), []);
    }

    #[test]
    fn events_build_the_status_of_each_host() {
        let mut certs = CertHosts::default();
        certs.apply(&stack("notes", &["notes.example.com"]));
        assert_eq!(
            certs.status(),
            BTreeMap::from([("notes.example.com".to_owned(), CertStatus::default())])
        );

        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let mismatch = DnsMismatch {
            host: "notes.example.com".into(),
            observed: vec![],
            expected: vec!["192.0.2.1".parse().unwrap()],
        };
        certs.record(&AcmeEvent::DnsMismatch(mismatch.clone()));
        certs.record(&AcmeEvent::Failed {
            host: "notes.example.com".into(),
            error: "dns".into(),
            retry_at: at,
        });
        let status = &certs.status()["notes.example.com"];
        assert_eq!(status.error.as_deref(), Some("dns"));
        assert_eq!(status.retry_at, Some(at));
        assert_eq!(status.dns_mismatch.as_ref(), Some(&mismatch));

        certs.record(&AcmeEvent::Issued {
            host: "notes.example.com".into(),
            not_after: at,
        });
        certs.record(&AcmeEvent::Issued {
            host: "admin.example.com".into(),
            not_after: at,
        });
        let issued = CertStatus {
            not_after: Some(at),
            dns_checked: true,
            ..CertStatus::default()
        };
        assert_eq!(certs.status()["notes.example.com"], issued);
        assert_eq!(certs.status()["admin.example.com"], issued);

        certs.remove("notes");
        assert!(!certs.status().contains_key("notes.example.com"));
    }

    #[test]
    fn a_dns_mismatch_lasts_until_an_attempt_finds_dns_pointing_here() {
        let mut certs = CertHosts::default();
        certs.apply(&stack("notes", &["notes.example.com"]));
        let at = SystemTime::UNIX_EPOCH;
        let failed = AcmeEvent::Failed {
            host: "notes.example.com".into(),
            error: "connection refused".into(),
            retry_at: at,
        };
        certs.record(&AcmeEvent::DnsMismatch(DnsMismatch {
            host: "notes.example.com".into(),
            observed: vec![],
            expected: vec!["192.0.2.1".parse().unwrap()],
        }));
        certs.record(&failed);
        assert!(certs.status()["notes.example.com"].dns_mismatch.is_some());

        // DNS was fixed, but the CA still fails: the mismatch is no longer the likely cause.
        certs.record(&failed);
        let status = &certs.status()["notes.example.com"];
        assert_eq!(status.dns_mismatch, None);
        assert_eq!(status.error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn a_host_issued_despite_a_mismatch_keeps_showing_it() {
        let mut certs = CertHosts::default();
        certs.apply(&stack("cdn", &["cdn.example.com"]));
        let at = SystemTime::UNIX_EPOCH;
        let mismatch = DnsMismatch {
            host: "cdn.example.com".into(),
            observed: vec!["203.0.113.9".parse().unwrap()],
            expected: vec!["192.0.2.1".parse().unwrap()],
        };
        let issued = AcmeEvent::Issued {
            host: "cdn.example.com".into(),
            not_after: at,
        };
        certs.record(&AcmeEvent::DnsMismatch(mismatch.clone()));
        certs.record(&issued);
        let status = &certs.status()["cdn.example.com"];
        assert_eq!(status.not_after, Some(at));
        assert_eq!(status.dns_mismatch.as_ref(), Some(&mismatch));

        // The renewal finds DNS pointing here.
        certs.record(&issued);
        assert_eq!(certs.status()["cdn.example.com"].dns_mismatch, None);
    }

    #[test]
    fn manual_dns_checks_update_routed_hosts_and_the_admin_host_only() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut certs = CertHosts::default();
        certs.apply(&stack("notes", &["notes.example.com"]));
        assert!(!certs.status()["notes.example.com"].dns_checked);

        let mismatch = DnsMismatch {
            host: "notes.example.com".into(),
            observed: vec!["203.0.113.9".parse().unwrap()],
            expected: vec!["192.0.2.1".parse().unwrap()],
        };
        certs.dns_checked(
            "Notes.example.com",
            None,
            &DnsVerdict::Mismatch(mismatch.clone()),
        );
        let status = &certs.status()["notes.example.com"];
        assert!(status.dns_checked);
        assert_eq!(status.dns_mismatch.as_ref(), Some(&mismatch));
        certs.dns_checked("notes.example.com", None, &DnsVerdict::Unknown);
        assert!(certs.status()["notes.example.com"].dns_mismatch.is_some());
        certs.dns_checked("notes.example.com", None, &DnsVerdict::Matches);
        assert_eq!(certs.status()["notes.example.com"].dns_mismatch, None);

        certs.dns_checked("other.example.com", None, &DnsVerdict::Matches);
        certs.dns_checked(
            "admin.example.com",
            Some("admin.example.com"),
            &DnsVerdict::Matches,
        );
        assert_eq!(
            certs.status().keys().collect::<Vec<_>>(),
            ["admin.example.com", "notes.example.com"]
        );

        assert!(!certs.retry("notes.example.com"), "no ACME task connected");
        certs.connect(tx);
        drain(&mut rx);
        assert!(certs.retry("NOTES.example.com"));
        assert_eq!(
            drain(&mut rx),
            [AcmeCommand::Retry {
                host: "notes.example.com".into()
            }]
        );
    }

    #[test]
    fn events_sent_before_a_forget_do_not_bring_the_host_back() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut certs = CertHosts::default();
        certs.connect(tx);
        certs.apply(&stack("notes", &["notes.example.com"]));
        certs.remove("notes");
        let issued = AcmeEvent::Issued {
            host: "notes.example.com".into(),
            not_after: SystemTime::UNIX_EPOCH,
        };
        // Issued while the Forget was still in the channel.
        certs.record(&issued);
        assert!(certs.status().is_empty());

        // Routed again, its events count again.
        certs.apply(&stack("notes", &["notes.example.com"]));
        certs.record(&issued);
        assert_eq!(
            certs.status()["notes.example.com"].not_after,
            Some(SystemTime::UNIX_EPOCH)
        );
    }
}
