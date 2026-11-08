//! ACME via instant-acme with HTTP-01 challenges (ADR-0015): the background issuance/renewal task
//! ([`AcmeTask`], run in production by [`AcmeService`]), on-disk certificate storage (ADR-0016)
//! and HTTP-01 issuance of one certificate per hostname ([`Issuer`]).
//!
//! instant-acme is built on ring only (ADR-0037); `scripts/check-no-aws-lc.sh` guards that in CI.

mod account;
mod atomic;
mod certs;
mod challenge;
mod issue;
mod task;

#[allow(
    unused_imports,
    reason = "part of the ACME API for its future callers (doctor, web UI)"
)]
pub use account::{AccountError, AccountStore};
#[allow(
    unused_imports,
    reason = "part of the ACME API for its future callers (doctor, web UI)"
)]
pub use certs::{CertEntry, CertError, CertMap, LoadReport, Validity};
pub use challenge::ChallengeMap;
#[allow(
    unused_imports,
    reason = "part of the ACME API for its future callers (doctor, web UI)"
)]
pub use issue::{IssueError, IssueFailure, Issuer};
pub use task::{AcmeCommand, AcmeEvent, AcmeService, DnsMismatch};
