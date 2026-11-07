//! ACME via instant-acme with HTTP-01 challenges (ADR-0015): the background issuance/renewal task
//! and on-disk certificate storage (ADR-0016).

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::acme"));
    }
}
