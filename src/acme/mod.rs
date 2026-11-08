//! ACME via instant-acme with HTTP-01 challenges (ADR-0015): the background issuance/renewal task
//! on-disk certificate storage (ADR-0016) and HTTP-01 issuance of one certificate per hostname
//! ([`Issuer`]).
//!
//! instant-acme is built on ring only (ADR-0037); `scripts/check-no-aws-lc.sh` guards that in CI.

#[allow(dead_code, reason = "consumed by the ACME task as it is implemented")]
mod account;
mod atomic;
#[allow(
    dead_code,
    reason = "insert and validity are consumed by the ACME task"
)]
mod certs;
#[allow(dead_code, reason = "insert and remove are consumed by the ACME task")]
mod challenge;
#[allow(dead_code, reason = "consumed by the ACME task as it is implemented")]
mod issue;

#[allow(
    unused_imports,
    reason = "consumed by the ACME task as it is implemented"
)]
pub use account::{AccountError, AccountStore};
#[allow(
    unused_imports,
    reason = "CertEntry and Validity are consumed by the ACME task"
)]
pub use certs::{CertEntry, CertError, CertMap, LoadReport, Validity};
pub use challenge::ChallengeMap;
#[allow(
    unused_imports,
    reason = "consumed by the ACME task as it is implemented"
)]
pub use issue::{IssueError, IssueFailure, Issuer};
