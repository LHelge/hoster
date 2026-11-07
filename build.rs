//! Rebuild when a migration changes: `sqlx::migrate!()` embeds the `migrations/` directory.

fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
