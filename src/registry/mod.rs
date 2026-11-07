//! Generic OCI registry client (token discovery, manifest `HEAD` for digests) and the
//! edge-triggered digest poller (ADR-0005, ADR-0043).

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::registry"));
    }
}
