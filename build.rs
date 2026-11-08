//! Build-time facts for the binary:
//! - rebuild when a migration changes: `sqlx::migrate!()` embeds the `migrations/` directory;
//! - `BUILD_TARGET_TRIPLE`, the target triple, which `hoster version --json` reports (ADR-0052).
//!   Not `HOSTER_*`: cargo also sets it when running tests, where that prefix is configuration.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=BUILD_TARGET_TRIPLE={target}");
}
