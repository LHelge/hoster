//! The `Orchestrator` handle (ADR-0050): deploy state machine, per-stack deploy tasks and the
//! reconciler that keeps containers running (ADR-0028).

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::orchestrator"));
    }
}
