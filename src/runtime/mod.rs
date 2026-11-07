//! The `ContainerRuntime` trait, its bollard-backed rootless podman implementation (ADR-0002) and a
//! fake for tests. Integration tests against a real podman are gated by an env var.

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::runtime"));
    }
}
