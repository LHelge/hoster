//! Edits to a stack file's text that keep everything else as written: comments, formatting,
//! key order (ADR-0061 §9).

use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item, Value};

use super::ImageRef;

/// New `image` values for some services of a stack file, by service name. Applied to the
/// file's text, each replaces the string in place, keeping its quotes' surroundings: a
/// trailing comment on the line stays, and nothing else in the file changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageEdits(BTreeMap<String, ImageRef>);

impl ImageEdits {
    /// Sets `service`'s image to `image`.
    pub fn set(&mut self, service: impl Into<String>, image: ImageRef) {
        self.0.insert(service.into(), image);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `toml` with the edits made. Fails when the text does not parse or a service, or its
    /// `image` string, is not in it.
    pub fn apply(&self, toml: &str) -> Result<String, String> {
        let mut doc: DocumentMut = toml
            .parse()
            .map_err(|e| format!("the stack file does not parse: {e}"))?;
        for (service, image) in &self.0 {
            let slot = doc
                .get_mut("services")
                .and_then(|services| services.get_mut(service))
                .and_then(|service| service.get_mut("image"))
                .and_then(Item::as_value_mut)
                .filter(|value| value.is_str())
                .ok_or_else(|| format!("the stack file has no `image` string for `{service}`"))?;
            let decor = slot.decor().clone();
            *slot = Value::from(image.to_string());
            *slot.decor_mut() = decor;
        }
        Ok(doc.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits(pairs: &[(&str, &str)]) -> ImageEdits {
        let mut edits = ImageEdits::default();
        for (service, image) in pairs {
            edits.set(*service, image.parse().unwrap());
        }
        edits
    }

    #[test]
    fn only_the_image_strings_change() {
        let toml = r#"# Photos, per the immich compose file.
version = 1
name = "photos"

[services.server]
image   =   "ghcr.io/immich-app/immich-server:v2.3.1"  # keep in step with ml
auto_update = "minor"

[services.ml]
image = 'ghcr.io/immich-app/immich-machine-learning:v2.3.1'

[services.db]
# floating major
image = "docker.io/library/postgres:16"
"#;
        let edited = edits(&[
            ("server", "ghcr.io/immich-app/immich-server:v2.4.0"),
            ("ml", "ghcr.io/immich-app/immich-machine-learning:v2.4.0"),
        ])
        .apply(toml)
        .unwrap();
        let expected = toml
            .replace(
                "\"ghcr.io/immich-app/immich-server:v2.3.1\"",
                "\"ghcr.io/immich-app/immich-server:v2.4.0\"",
            )
            .replace(
                "'ghcr.io/immich-app/immich-machine-learning:v2.3.1'",
                "\"ghcr.io/immich-app/immich-machine-learning:v2.4.0\"",
            );
        assert_eq!(edited, expected);
    }

    #[test]
    fn inline_and_dotted_services_are_edited_too() {
        let toml = "version = 1\nname = \"a\"\nservices.web = { image = \"docker.io/library/nginx:1.27\" }\n\n[services.db]\nimage = \"docker.io/library/postgres:16\"\n";
        let digest = format!("sha256:{}", "a".repeat(64));
        let edited = edits(&[
            ("web", "docker.io/library/nginx:1.28"),
            ("db", &format!("docker.io/library/postgres:16@{digest}")),
        ])
        .apply(toml)
        .unwrap();
        assert!(
            edited.contains("services.web = { image = \"docker.io/library/nginx:1.28\" }"),
            "{edited}"
        );
        assert!(
            edited.contains(&format!(
                "image = \"docker.io/library/postgres:16@{digest}\""
            )),
            "{edited}"
        );
    }

    #[test]
    fn missing_services_and_bad_files_fail() {
        let edits = edits(&[("gone", "docker.io/library/nginx:1.28")]);
        assert!(
            edits
                .apply("[services.web]\nimage = \"x\"\n")
                .unwrap_err()
                .contains("`gone`")
        );
        assert!(
            edits
                .apply("[services.gone]\nimage = 1\n")
                .unwrap_err()
                .contains("`gone`")
        );
        assert!(edits.apply("[[").unwrap_err().contains("does not parse"));
        assert_eq!(
            ImageEdits::default().apply("a = 1 # c\n").unwrap(),
            "a = 1 # c\n"
        );
    }
}
