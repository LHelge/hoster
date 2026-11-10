//! The help beside the stack editor (ADR-0060, epic M15): a field reference rendered from the
//! stack file's JSON Schema, generic snippets, and the starter skeleton of an empty New stack
//! editor. Nothing here is app-specific (ADR-0018): the snippets show the format, not an app.
//!
//! The reference walks the schema from its root, so a field added to the stack format (and so to
//! the schema, which a drift test keeps in step with the parser) shows up here without a change.

use std::sync::LazyLock;

use serde_json::{Map, Value};

use crate::stack::StackSchema;

/// Everything the apply page shows beside the editor. Built once.
pub(super) struct EditorHelp {
    /// The reference, one table per TOML table of the format, the top level first.
    pub(super) tables: Vec<RefTable>,
    pub(super) snippets: &'static [Snippet],
}

impl EditorHelp {
    /// What an empty New stack editor starts with: the `#:schema` line for local editors, and a
    /// minimal stack whose blanks are explained in comments. Applied unchanged it fails, with
    /// a diagnostic on each blank, so it never creates a stack by accident.
    pub(super) const SKELETON: &str = concat!(
        "#:schema https://docs.hoster.lhelge.se/stack.schema.json\n",
        "# The services that deploy together, on one network.\n",
        "# Snippets and the field reference are beside the editor.\n",
        "version = 1\n",
        "# A lowercase letter, then letters, digits or -.\n",
        "name = \"\"\n",
        "\n",
        "[services.web]\n",
        "# Built in CI: \"ghcr.io/you/app\"\n",
        "# Third-party: \"docker.io/library/nginx:1.27\"\n",
        "image = \"\"\n",
        "\n",
        "# A route reaches the service on a hostname, over HTTPS:\n",
        "# [[services.web.routes]]\n",
        "# host = \"app.example.com\"\n",
        "# port = 8080\n",
    );

    pub(super) fn get() -> &'static EditorHelp {
        static HELP: LazyLock<EditorHelp> = LazyLock::new(|| EditorHelp {
            tables: RefTable::all(StackSchema::get().as_ref()),
            snippets: Snippet::ALL,
        });
        &HELP
    }
}

/// One TOML table in the reference.
pub(super) struct RefTable {
    /// The table's header, e.g. `[services.<service>.health]`; `None` for the top level.
    pub(super) header: Option<String>,
    pub(super) about: Prose,
    /// Required fields first, then the rest by name.
    pub(super) fields: Vec<RefField>,
}

/// One field of a [`RefTable`].
pub(super) struct RefField {
    pub(super) name: String,
    /// What it takes: `string`, `integer`, `array of strings`, `table`, `"a" | "b"`, …
    pub(super) kind: String,
    /// Its default as TOML, when the schema gives one worth showing.
    pub(super) default: Option<String>,
    pub(super) required: bool,
    pub(super) about: Prose,
}

impl RefTable {
    /// Every table of the stack format, from its JSON Schema.
    fn all(schema: &str) -> Vec<RefTable> {
        let root: Value = serde_json::from_str(schema).expect("the stack schema is JSON");
        let empty = Map::new();
        let defs = root
            .get("$defs")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let mut tables = Vec::new();
        Walk { defs }.table(None, "", &root, &mut tables);
        tables
    }
}

/// A walk over the schema, following `$ref`s into its `$defs`.
struct Walk<'a> {
    defs: &'a Map<String, Value>,
}

impl<'a> Walk<'a> {
    /// `schema`, or the definition it refers to.
    fn resolve(&self, schema: &'a Value) -> &'a Value {
        schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| self.defs.get(name))
            .unwrap_or(schema)
    }

    /// Whether `schema` is a table with named fields.
    fn is_table(schema: &Value) -> bool {
        schema.get("properties").is_some_and(Value::is_object)
    }

    /// Adds the table `schema` at `path` (dotted, empty at the top level), then the tables
    /// below it.
    fn table(
        &self,
        header: Option<String>,
        path: &str,
        schema: &'a Value,
        tables: &mut Vec<RefTable>,
    ) {
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut fields = Vec::new();
        let mut below = Vec::new();
        let properties = schema.get("properties").and_then(Value::as_object);
        for (name, property) in properties.into_iter().flatten() {
            let target = self.resolve(property);
            let at = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}.{name}")
            };
            let items = target.get("items").map(|items| self.resolve(items));
            let values = target
                .get("additionalProperties")
                .map(|values| self.resolve(values));
            let kind = if Self::is_table(target) {
                below.push((format!("[{at}]"), at, target));
                "table".to_owned()
            } else if let Some(items) = items.filter(|i| Self::is_table(i)) {
                below.push((format!("[[{at}]]"), at, items));
                "array of tables".to_owned()
            } else if let Some(values) = values.filter(|v| Self::is_table(v)) {
                let key = Self::placeholder(name);
                let at = format!("{at}.{key}");
                below.push((format!("[{at}]"), at, values));
                format!("tables, one per {key}")
            } else {
                Self::kind(target, items, values)
            };
            let default = property
                .get("default")
                .or_else(|| target.get("default"))
                .and_then(Self::toml);
            let about = property
                .get("description")
                .or_else(|| target.get("description"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            fields.push(RefField {
                name: name.clone(),
                kind,
                default,
                required: required.contains(&name.as_str()),
                about: Prose::from(about),
            });
        }
        fields.sort_by(|a, b| b.required.cmp(&a.required).then(a.name.cmp(&b.name)));
        let about = schema
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        tables.push(RefTable {
            header,
            about: Prose::summary(about),
            fields,
        });
        for (header, at, schema) in below {
            self.table(Some(header), &at, schema, tables);
        }
    }

    /// What a field that is not a table takes.
    fn kind(target: &Value, items: Option<&Value>, values: Option<&Value>) -> String {
        let consts: Vec<String> = target
            .get("oneOf")
            .and_then(Value::as_array)
            .map(|variants| {
                variants
                    .iter()
                    .filter_map(|v| v.get("const").and_then(Value::as_str))
                    .map(|c| format!("\"{c}\""))
                    .collect()
            })
            .unwrap_or_default();
        if !consts.is_empty() {
            return consts.join(" | ");
        }
        match Self::type_of(Some(target)) {
            "array" => format!("array of {}s", Self::type_of(items)),
            "object" => format!("table of {}s", Self::type_of(values)),
            other => other.to_owned(),
        }
    }

    /// A schema's JSON type, or `value` when it has none.
    fn type_of(schema: Option<&Value>) -> &str {
        schema
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("value")
    }

    /// The placeholder for the keys of a table of tables, as the header shows it.
    fn placeholder(name: &str) -> &'static str {
        match name {
            "services" => "<service>",
            "files" => "\"<path>\"",
            _ => "<name>",
        }
    }

    /// A default as TOML. Empty arrays and tables are left out: they only say "none".
    fn toml(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(format!("{s:?}")),
            Value::Bool(b) => Some(b.to_string()),
            Value::Number(n) => Some(n.to_string()),
            Value::Array(a) if a.is_empty() => None,
            Value::Object(o) if o.is_empty() => None,
            _ => None,
        }
    }
}

/// A schema description as text with `code` spans: the line wraps of the doc comments it comes
/// from joined into spaces.
pub(super) struct Prose(pub(super) Vec<ProseSpan>);

/// A run of [`Prose`], plain or code.
pub(super) struct ProseSpan {
    pub(super) code: bool,
    pub(super) text: String,
}

impl Prose {
    /// The first paragraph of `text`: a table's description says what the table is, and its
    /// fields say the rest.
    fn summary(text: &str) -> Self {
        Prose::from(text.split("\n\n").next().unwrap_or_default())
    }
}

impl From<&str> for Prose {
    fn from(text: &str) -> Self {
        let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let parts: Vec<&str> = joined.split('`').collect();
        // An unmatched backtick: the rest is plain text, backtick included.
        let balanced = parts.len() % 2 == 1;
        let mut spans = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let code = i % 2 == 1 && (balanced || i + 1 < parts.len());
            let text = if i % 2 == 1 && !code {
                format!("`{part}")
            } else {
                (*part).to_owned()
            };
            if !text.is_empty() {
                spans.push(ProseSpan { code, text });
            }
        }
        Prose(spans)
    }
}

/// A generic piece of stack file the editor can insert at the caret.
pub(super) struct Snippet {
    /// Used in element ids: `snippet-<id>`.
    pub(super) id: &'static str,
    pub(super) title: &'static str,
    pub(super) about: &'static str,
    pub(super) toml: &'static str,
    /// It is a table of a service, written for `services.web`: the editor script retargets it
    /// to the service the caret is in.
    pub(super) per_service: bool,
}

impl Snippet {
    pub(super) const ALL: &[Snippet] = &[
        Snippet {
            id: "service",
            title: "Service",
            about: "Another container in the stack. The others reach it by its name.",
            toml: "[services.worker]\nimage = \"ghcr.io/you/worker\"\n# depends_on = [\"web\"]\n# env = { LOG_LEVEL = \"info\" }\n",
            per_service: false,
        },
        Snippet {
            id: "route",
            title: "Route",
            about: "Makes the service reachable on a hostname, over HTTPS. hoster requests the \
                    certificate when the stack is applied.",
            toml: "[[services.web.routes]]\nhost = \"app.example.com\"\nport = 8080\n# prefix = \"/api\"\n",
            per_service: true,
        },
        Snippet {
            id: "volume",
            title: "Volume",
            about: "A named volume for data that outlives a deploy. It is kept when the stack \
                    is removed.",
            toml: "[services.web.volumes]\ndata = \"/data\"\n",
            per_service: true,
        },
        Snippet {
            id: "health-http",
            title: "HTTP health check",
            about: "For a service with a route: a GET that must answer 2xx before traffic \
                    moves to a new container.",
            toml: "[services.web.health]\nhttp = { path = \"/healthz\" }\nstart_period = \"30s\"\n",
            per_service: true,
        },
        Snippet {
            id: "health-exec",
            title: "Command health check",
            about: "For a service without a route, such as a database or a worker: a command \
                    run in the container, without a shell. Exit 0 passes.",
            toml: "[services.web.health]\nexec = [\"/app/healthcheck\"]\n",
            per_service: true,
        },
        Snippet {
            id: "files",
            title: "Config file",
            about: "A file written into the container before it starts, from the stack file \
                    or from a secret.",
            toml: "[services.web.files.\"/etc/app/config.toml\"]\ncontent = \"\"\"\nkey = \"value\"\n\"\"\"\n# Or the value of a secret set on the environment tab:\n# secret = \"<stack>/<name>\"\n",
            per_service: true,
        },
        Snippet {
            id: "deploy",
            title: "Deploy from CI",
            about: "Lets GitHub Actions in one repository deploy new image digests with its \
                    OIDC token.",
            toml: "[deploy]\nrepository_id = 123456789  # GitHub's numeric repository id\n# refs = [\"refs/heads/main\"]\n",
            per_service: false,
        },
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::Stack;

    /// The skeleton, filled in, with a route on `web`: what the snippets are tried against.
    fn filled() -> String {
        format!(
            "{}\n[[services.web.routes]]\nhost = \"app.example.com\"\nport = 8080\n",
            EditorHelp::SKELETON
                .replace("name = \"\"", "name = \"notes\"")
                .replace("image = \"\"", "image = \"ghcr.io/you/app\"")
        )
    }

    #[test]
    fn every_schema_field_is_in_the_reference() {
        let schema: Value = serde_json::from_str(StackSchema::get().as_ref()).unwrap();
        let help = EditorHelp::get();
        let names = |table: &RefTable| -> Vec<String> {
            table.fields.iter().map(|f| f.name.clone()).collect()
        };
        let top = &help.tables[0];
        assert!(top.header.is_none());
        let mut expected: Vec<String> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        expected.sort();
        let mut got = names(top);
        got.sort();
        assert_eq!(got, expected);
        // Every definition with fields is a table of the reference, with all of them.
        for (def, body) in schema["$defs"].as_object().unwrap() {
            let Some(properties) = body.get("properties").and_then(Value::as_object) else {
                continue;
            };
            let mut expected: Vec<String> = properties.keys().cloned().collect();
            expected.sort();
            let found = help.tables.iter().any(|table| {
                let mut got = names(table);
                got.sort();
                got == expected
            });
            assert!(found, "no table lists the fields of {def}: {expected:?}");
        }
    }

    #[test]
    fn the_reference_names_tables_by_their_toml_headers() {
        let headers: Vec<_> = EditorHelp::get()
            .tables
            .iter()
            .filter_map(|t| t.header.clone())
            .collect();
        for header in [
            "[deploy]",
            "[services.<service>]",
            "[[services.<service>.routes]]",
            "[services.<service>.routes.timeouts]",
            "[services.<service>.health]",
            "[services.<service>.health.http]",
            "[services.<service>.files.\"<path>\"]",
        ] {
            assert!(
                headers.contains(&header.to_owned()),
                "{header} in {headers:?}"
            );
        }
        let service = EditorHelp::get()
            .tables
            .iter()
            .find(|t| t.header.as_deref() == Some("[services.<service>]"))
            .unwrap();
        let field = |name: &str| service.fields.iter().find(|f| f.name == name).unwrap();
        assert!(field("image").required);
        assert_eq!(
            service.fields[0].name, "image",
            "required fields come first"
        );
        assert_eq!(field("update").kind, r#""start-first" | "stop-first""#);
        assert_eq!(field("update").default.as_deref(), Some(r#""start-first""#));
        assert_eq!(field("depends_on").kind, "array of strings");
        assert_eq!(
            field("depends_on").default,
            None,
            "an empty default says nothing"
        );
        assert_eq!(field("env").kind, "table of strings");
        assert_eq!(field("routes").kind, "array of tables");
        assert_eq!(field("files").kind, "tables, one per \"<path>\"");
        assert_eq!(field("read_only").default.as_deref(), Some("false"));
    }

    #[test]
    fn prose_keeps_code_spans_and_joins_line_wraps() {
        let prose = Prose::from("Use `[\"sh\", \"-c\"]` when\nit needs a shell, or `x");
        let spans: Vec<_> = prose.0.iter().map(|s| (s.code, s.text.as_str())).collect();
        assert_eq!(
            spans,
            [
                (false, "Use "),
                (true, "[\"sh\", \"-c\"]"),
                (false, " when it needs a shell, or "),
                (false, "`x"),
            ]
        );
    }

    #[test]
    fn the_skeleton_fits_the_editor_without_scrolling() {
        for line in EditorHelp::SKELETON.lines() {
            assert!(
                line.chars().count() <= 60,
                "too wide for the editor: {line}"
            );
        }
    }

    #[test]
    fn the_skeleton_is_toml_that_fails_on_its_blanks() {
        assert!(toml::de::DeTable::parse(EditorHelp::SKELETON).is_ok());
        assert!(EditorHelp::SKELETON.starts_with(&format!("#:schema {}\n", StackSchema::URL)));
        let err = Stack::parse(EditorHelp::SKELETON).unwrap_err();
        let lines: Vec<_> = err
            .diagnostics()
            .iter()
            .map(|d| (d.path.to_string(), d.location.map(|l| l.line)))
            .collect();
        assert_eq!(
            lines,
            [
                ("name".to_owned(), Some(6)),
                ("services.web.image".to_owned(), Some(11)),
            ],
            "{err}"
        );
        // Filled in, it is a stack.
        Stack::parse(&filled()).unwrap();
    }

    #[test]
    fn every_snippet_parses_in_a_stack() {
        for snippet in Snippet::ALL {
            let toml = format!("{}\n{}", filled(), snippet.toml);
            if let Err(err) = Stack::parse(&toml) {
                panic!("snippet {}: {err}\n{toml}", snippet.id);
            }
            assert!(snippet.toml.ends_with('\n'), "{}", snippet.id);
            assert_eq!(
                snippet.per_service,
                snippet.toml.starts_with("[services.web.")
                    || snippet.toml.starts_with("[[services.web."),
                "{}: per-service snippets are written for services.web",
                snippet.id
            );
        }
    }
}
