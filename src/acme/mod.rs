//! ACME via instant-acme with HTTP-01 challenges (ADR-0015): the background issuance/renewal task
//! and on-disk certificate storage (ADR-0016).
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
