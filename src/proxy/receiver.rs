//! The OTLP receiver's route on `:80` (ADR-0069 §3): a request for
//! [`OtlpReceiver::HOST`](crate::otlp::OtlpReceiver::HOST) whose peer is one of this host's own
//! addresses is proxied to the receiver's Unix socket, as the admin host is to the admin app's
//! (ADR-0031). The same host from any other peer is an unknown host, answered 404 (ADR-0032).
//!
//! The peer check keeps outside clients out: the kernel drops a packet from outside that claims
//! a local source address. It is not identity: every container (pasta delivers a container's
//! connection to `169.254.1.2` from the host's interface address, spike `h4q`), every process
//! on the host and other users' containers pass it; the receiver's token decides who speaks.
//!
//! **Which addresses are the host's own** is decided per receiver request, never cached, so it
//! cannot go stale when interfaces change:
//!
//! 1. A peer that is the connection's own local address is local: an accepted socket's local
//!    address is one of the host's, and this is how every container and most host processes
//!    arrive. No system call.
//! 2. Otherwise [`HostAddresses`] answers: loopback is always local; anything else is compared
//!    with the interfaces' addresses as `getifaddrs` lists them now (one netlink dump). That
//!    happens only for a request that names the receiver's host from a peer that is not its
//!    own local address: a host process with another source address, or an outside client
//!    probing, and an outside client pays the same in its own round trips.
//!
//! An IPv4-mapped IPv6 peer (a dual-stack listener) is unmapped first.

use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::otlp::OtlpReceiver;
use crate::routing::{Owner, RouteEntry, RouteTable, Target, UnixUpstream};
use crate::stack::{NormalizedPrefix, RouteTimeouts};

/// Which addresses are this host's own.
pub trait HostAddresses: Send + Sync + fmt::Debug {
    /// Whether `ip`, never an IPv4-mapped IPv6 address, is one of them.
    fn contains(&self, ip: IpAddr) -> bool;
}

/// The host's addresses as its interfaces have them at the time of each question, and every
/// loopback address.
#[derive(Debug, Default, Clone, Copy)]
pub struct Interfaces;

impl HostAddresses for Interfaces {
    fn contains(&self, ip: IpAddr) -> bool {
        if ip.is_loopback() {
            return true;
        }
        match nix::ifaddrs::getifaddrs() {
            Ok(interfaces) => interfaces
                .filter_map(|interface| interface.address)
                .filter_map(|address| {
                    address
                        .as_sockaddr_in()
                        .map(|v4| IpAddr::V4(v4.ip()))
                        .or_else(|| address.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip())))
                })
                .any(|own| own == ip),
            Err(err) => {
                // Refusing is the safe answer: the request gets the unknown host's 404.
                tracing::warn!(error = %err, "cannot list the host's addresses");
                false
            }
        }
    }
}

/// Where the `:80` service sends receiver requests, and which peers may send them.
#[derive(Debug, Clone)]
pub struct ReceiverRoute {
    /// The receiver's socket, as a reserved route of hoster's own: the access log names it,
    /// and no request row is written for it (ADR-0067 §5).
    entry: RouteEntry,
    addresses: Arc<dyn HostAddresses>,
}

impl ReceiverRoute {
    /// What the access log names the route's owner.
    pub const PURPOSE: &str = "OTLP receiver";

    /// The route to the receiver listening on `socket`, for peers among the host's own
    /// [`Interfaces`].
    pub fn new(socket: PathBuf) -> Self {
        Self {
            entry: RouteEntry {
                owner: Owner::Reserved(Self::PURPOSE),
                prefix: NormalizedPrefix::root(),
                prefix_str: "/".to_owned(),
                strip_prefix: false,
                target: Target::Unix(Arc::new(UnixUpstream::from(socket))),
                // The proxy's defaults: exports are short requests.
                timeouts: RouteTimeouts::default(),
            },
            addresses: Arc::new(Interfaces),
        }
    }

    /// The same route, deciding with `addresses` which peers are local; for tests, which
    /// cannot connect from an address that is not the host's own.
    #[cfg(test)]
    pub(crate) fn with_host_addresses(self, addresses: Arc<dyn HostAddresses>) -> Self {
        Self { addresses, ..self }
    }

    /// The route to the receiver, if a request for `host` (as sent) arrived from `peer` on a
    /// connection whose local address is `local`.
    pub(super) fn route(
        &self,
        host: &str,
        peer: Option<IpAddr>,
        local: Option<IpAddr>,
    ) -> Option<&RouteEntry> {
        if RouteTable::normalize_host(host) != OtlpReceiver::HOST {
            return None;
        }
        let peer = peer?.to_canonical();
        let local = local.map(|ip| ip.to_canonical());
        (local == Some(peer) || self.addresses.contains(peer)).then_some(&self.entry)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    /// Says the host's own addresses are `127.0.0.2` and `2001:db8::2`, and nothing else.
    #[derive(Debug)]
    struct Fixed;

    impl HostAddresses for Fixed {
        fn contains(&self, ip: IpAddr) -> bool {
            ip == Ipv4Addr::new(127, 0, 0, 2) || ip == "2001:db8::2".parse::<Ipv6Addr>().unwrap()
        }
    }

    fn route() -> ReceiverRoute {
        ReceiverRoute::new(PathBuf::from("/run/otlp.sock")).with_host_addresses(Arc::new(Fixed))
    }

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn only_the_receivers_host_from_a_local_peer_is_routed() {
        let route = route();
        let local = ip("10.0.0.5");
        for host in [
            "host.containers.internal",
            "HOST.containers.internal.",
            "host.containers.internal:80",
        ] {
            assert!(
                route.route(host, ip("127.0.0.2"), local).is_some(),
                "{host}"
            );
        }
        for host in ["host.docker.internal", "app.example.com", ""] {
            assert!(
                route.route(host, ip("127.0.0.2"), local).is_none(),
                "{host}"
            );
        }
        let host = OtlpReceiver::HOST;
        // Not one of the host's addresses, and not the connection's own.
        assert!(route.route(host, ip("203.0.113.9"), local).is_none());
        assert!(route.route(host, ip("2001:db8::3"), local).is_none());
        assert!(route.route(host, None, local).is_none());
        // The connection's own local address is the host's, whatever the list says.
        assert!(route.route(host, ip("10.0.0.5"), local).is_some());
        assert!(route.route(host, ip("2001:db8::2"), local).is_some());
    }

    #[test]
    fn ipv4_mapped_peers_are_unmapped_first() {
        let route = route();
        let host = OtlpReceiver::HOST;
        let local = ip("::ffff:10.0.0.5");
        assert!(route.route(host, ip("::ffff:127.0.0.2"), None).is_some());
        assert!(route.route(host, ip("::ffff:10.0.0.5"), local).is_some());
        assert!(
            route
                .route(host, ip("::ffff:10.0.0.5"), ip("10.0.0.5"))
                .is_some()
        );
        assert!(route.route(host, ip("::ffff:203.0.113.9"), local).is_none());
    }

    #[test]
    fn the_route_is_hosters_own_and_goes_to_the_socket() {
        let route = route();
        let entry = route
            .route(OtlpReceiver::HOST, ip("127.0.0.2"), None)
            .unwrap();
        assert_eq!(entry.owner, Owner::Reserved("OTLP receiver"));
        assert!(
            matches!(&entry.target, Target::Unix(u) if u.as_ref().as_ref() == std::path::Path::new("/run/otlp.sock"))
        );
    }

    #[test]
    fn the_interfaces_include_loopback_and_this_hosts_addresses() {
        let interfaces = Interfaces;
        assert!(interfaces.contains(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(interfaces.contains(IpAddr::V4(Ipv4Addr::new(127, 9, 9, 9))));
        assert!(interfaces.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // TEST-NET-3 is never assigned to a real interface.
        assert!(!interfaces.contains(ip("203.0.113.77").unwrap()));
        // Every address an interface has now counts, read at the time of the question.
        for interface in nix::ifaddrs::getifaddrs().unwrap() {
            if let Some(v4) = interface.address.and_then(|a| a.as_sockaddr_in().copied()) {
                assert!(interfaces.contains(IpAddr::V4(v4.ip())), "{v4}");
            }
        }
    }
}
