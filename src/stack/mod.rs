//! Stack file model, parsing and validation (ADR-0006, `docs/stack-format.md`).
//!
//! Pure: no I/O beyond the input text, so it carries most of the unit tests.

#[cfg(test)]
mod tests {
    #[test]
    fn module_is_wired_into_the_crate() {
        assert!(module_path!().starts_with("hoster::stack"));
    }
}
