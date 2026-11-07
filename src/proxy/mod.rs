//! The embedded Pingora proxy on its own thread (ADR-0008): the `:80` and `:443` services and the
//! rustls `CertResolver` (ADR-0014).

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::proxy"));
    }
}
