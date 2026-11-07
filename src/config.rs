//! Daemon configuration: `~/.config/hoster/hoster.toml` with `HOSTER_*` environment overrides
//! (ADR-0024). Secrets are never read from here; they live in separate files or credentials.

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::config"));
    }
}
