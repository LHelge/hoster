//! `CertResolver`: the rustls certificate selection for the `:443` listener (ADR-0014,
//! docs/architecture.md §7.1), installed with `TlsSettings::set_cert_resolver`.

use std::sync::Arc;

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::acme::CertMap;

/// Picks the certificate for a handshake by SNI from the current [`CertMap`] snapshot.
///
/// There is no default certificate (ADR-0032): a handshake without SNI, or for a host hoster has
/// no certificate for, gets `None` and fails.
#[derive(Debug, Clone)]
pub struct CertResolver {
    certs: CertMap,
}

impl From<CertMap> for CertResolver {
    fn from(certs: CertMap) -> Self {
        Self { certs }
    }
}

impl CertResolver {
    /// The certificate for the SNI name `server_name`, if any; the lookup ignores case.
    fn resolve_name(&self, server_name: Option<&str>) -> Option<Arc<CertifiedKey>> {
        self.certs.get(server_name?)
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let server_name = client_hello.server_name();
        let key = self.resolve_name(server_name);
        if key.is_none() {
            tracing::debug!(
                sni = server_name,
                "no certificate for SNI; failing handshake"
            );
        }
        key
    }
}

#[cfg(test)]
mod tests {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
    use rustls::pki_types::{CertificateDer, ServerName};
    use rustls::{
        ClientConfig, ClientConnection, Connection, RootCertStore, ServerConfig, ServerConnection,
    };

    use super::*;

    fn cert(host: &str) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec![host.to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn resolves_by_lowercased_sni_and_misses_are_none() {
        let dir = tempfile::tempdir().unwrap();
        let (certs, _) = CertMap::open(dir.path(), None).unwrap();
        let (a_chain, a_key) = cert("a.example.com");
        let (b_chain, b_key) = cert("b.example.com");
        certs.insert("a.example.com", a_chain, a_key).unwrap();
        certs.insert("b.example.com", b_chain, b_key).unwrap();
        let resolver = CertResolver::from(certs.clone());

        let a = resolver
            .resolve_name(Some("A.Example.COM"))
            .expect("a resolves");
        let b = resolver
            .resolve_name(Some("b.example.com"))
            .expect("b resolves");
        assert!(Arc::ptr_eq(&a, &certs.get("a.example.com").unwrap()));
        assert!(Arc::ptr_eq(&b, &certs.get("b.example.com").unwrap()));
        assert!(!Arc::ptr_eq(&a, &b));

        assert!(resolver.resolve_name(Some("c.example.com")).is_none());
        assert!(
            resolver.resolve_name(None).is_none(),
            "no default certificate"
        );
    }

    #[test]
    fn sees_certificates_inserted_after_it_was_built() {
        let dir = tempfile::tempdir().unwrap();
        let (certs, _) = CertMap::open(dir.path(), None).unwrap();
        let resolver = CertResolver::from(certs.clone());
        assert!(resolver.resolve_name(Some("late.example.com")).is_none());
        let (chain, key) = cert("late.example.com");
        certs.insert("late.example.com", chain, key).unwrap();
        assert!(resolver.resolve_name(Some("late.example.com")).is_some());
    }

    /// A leaf for `host` signed by a fresh CA, as (fullchain PEM, key PEM, CA DER).
    fn issued(host: &str) -> (String, String, CertificateDer<'static>) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name.push(DnType::CommonName, "Test CA");
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec![host.to_owned()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        (
            format!("{}{}", leaf.pem(), ca_cert.pem()),
            key.serialize_pem(),
            ca_cert.der().clone(),
        )
    }

    /// Runs a TLS handshake in memory between a server using `resolver` and a ring client
    /// trusting `roots`, connecting to `name` (with or without SNI). Returns the leaf the client
    /// was served.
    fn handshake(
        resolver: CertResolver,
        roots: &[CertificateDer<'static>],
        name: &str,
        sni: bool,
    ) -> Result<CertificateDer<'static>, rustls::Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver));
        let mut root_store = RootCertStore::empty();
        for root in roots {
            root_store.add(root.clone())?;
        }
        let mut client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(root_store)
            .with_no_client_auth();
        client_config.enable_sni = sni;

        let mut server = Connection::from(ServerConnection::new(Arc::new(server_config))?);
        let mut client = Connection::from(ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from(name.to_owned()).unwrap(),
        )?);
        fn transfer(from: &mut Connection, to: &mut Connection) -> Result<(), rustls::Error> {
            let mut bytes = Vec::new();
            while from.wants_write() {
                from.write_tls(&mut bytes).unwrap();
            }
            let mut rest = bytes.as_slice();
            while !rest.is_empty() {
                to.read_tls(&mut rest).unwrap();
            }
            to.process_new_packets().map(|_| ())
        }
        for _ in 0..10 {
            if !client.is_handshaking() && !server.is_handshaking() {
                break;
            }
            transfer(&mut client, &mut server)?;
            transfer(&mut server, &mut client)?;
        }
        assert!(!client.is_handshaking(), "handshake did not finish");
        Ok(client.peer_certificates().unwrap()[0].clone())
    }

    #[test]
    fn tls_handshake_serves_the_certificate_for_the_sni_name() {
        let dir = tempfile::tempdir().unwrap();
        let (certs, _) = CertMap::open(dir.path(), None).unwrap();
        let (a_chain, a_key, a_ca) = issued("a.example.com");
        let (b_chain, b_key, b_ca) = issued("b.example.com");
        certs.insert("a.example.com", a_chain, a_key).unwrap();
        certs.insert("b.example.com", b_chain, b_key).unwrap();
        let roots = [a_ca, b_ca];

        for host in ["a.example.com", "b.example.com"] {
            let served = handshake(CertResolver::from(certs.clone()), &roots, host, true)
                .unwrap_or_else(|e| panic!("{host}: {e}"));
            let expected = certs.get(host).unwrap();
            assert_eq!(&served, expected.end_entity_cert().unwrap(), "{host}");
        }
    }

    #[test]
    fn tls_handshake_fails_for_unknown_or_missing_sni() {
        let dir = tempfile::tempdir().unwrap();
        let (certs, _) = CertMap::open(dir.path(), None).unwrap();
        let (chain, key, ca) = issued("a.example.com");
        certs.insert("a.example.com", chain, key).unwrap();
        let roots = [ca];

        // The server refuses (rather than the client rejecting a wrong certificate).
        let refused = rustls::Error::General("no server certificate chain resolved".to_owned());
        let resolver = || CertResolver::from(certs.clone());
        assert_eq!(
            handshake(resolver(), &roots, "c.example.com", true).unwrap_err(),
            refused
        );
        assert_eq!(
            handshake(resolver(), &roots, "a.example.com", false).unwrap_err(),
            refused,
            "no default certificate without SNI"
        );
    }
}
