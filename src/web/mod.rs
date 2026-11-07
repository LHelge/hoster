//! The Axum app (ADR-0021): admin UI routes, GitHub OAuth and sessions (ADR-0031), and the deploy
//! webhook handler (ADR-0042, ADR-0047).

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::web"));
    }
}
