//! Generic OCI registry client (token discovery, manifest `HEAD` for digests) and the
//! edge-triggered digest poller (ADR-0005, ADR-0043).
//!
//! Also home to the registry credentials (ADR-0025): one credential per registry host, shared
//! by podman pulls (`X-Registry-Auth`) and the registry client's token requests.

mod credentials;

pub use credentials::{
    Anonymous, CredentialError, FileCredentials, RegistryCredential, RegistryCredentials,
};
