//! Generic OCI registry client (token discovery, manifest `HEAD` for digests, paginated tag
//! lists) and the edge-triggered digest poller (ADR-0005, ADR-0043, ADR-0061).
//!
//! Also home to the registry credentials (ADR-0025): one credential per registry host, kept in
//! the secret store and shared by podman pulls (`X-Registry-Auth`) and the registry client's
//! token requests.

#[allow(
    dead_code,
    reason = "consumed by stack apply and the poller as they are implemented"
)]
mod client;
mod credentials;
mod poller;

#[allow(
    unused_imports,
    reason = "consumed by stack apply and the poller as they are implemented"
)]
pub use client::{RegistryCheck, RegistryClient, RegistryError};
pub use credentials::{
    Anonymous, CredentialError, RegistryCredential, RegistryCredentials, RegistryEntry,
    RegistryHost, StoredCredentials,
};
pub use poller::{Poller, TrackedService};
