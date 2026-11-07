//! `RouteTable`: host + longest segment-boundary prefix matching (ADR-0009) and the cross-stack
//! (host, prefix) collision check (ADR-0012).
//!
//! Pure: no I/O, so it carries most of the unit tests.

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::routing"));
    }
}
