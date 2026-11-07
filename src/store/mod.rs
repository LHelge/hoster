//! State store: SQLite via sqlx (ADR-0022). Migrations and the `.sqlx/` offline cache live at the
//! crate root.

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::store"));
    }
}
