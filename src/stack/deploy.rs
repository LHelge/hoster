//! The `[deploy]` rule (ADR-0047, stack-format.md §2.5) and its check against a verified
//! GitHub Actions caller.

use std::fmt;

/// Webhook deploy authorization, matched against GitHub Actions OIDC claims (ADR-0047).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployAuth {
    pub repository_id: u64,
    /// Glob patterns over the token's `ref`; empty means any ref.
    pub refs: Vec<String>,
    pub environment: Option<String>,
    pub job_workflow_ref: Option<String>,
}

/// Who asks to deploy: the claims of a verified OIDC token that a `[deploy]` rule restricts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployCaller {
    pub repository_id: u64,
    /// The token's `ref`, e.g. `refs/tags/v1.4.2`; empty when the token carries none.
    pub git_ref: String,
    pub environment: Option<String>,
    pub job_workflow_ref: Option<String>,
}

/// Why a [`DeployAuth`] refused a [`DeployCaller`]. Shown to the admin with the rejected
/// attempt; never sent to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployDenied {
    Repository {
        expected: u64,
        actual: u64,
    },
    Ref(String),
    Environment {
        expected: String,
        actual: Option<String>,
    },
    Workflow {
        expected: String,
        actual: Option<String>,
    },
}

impl fmt::Display for DeployDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let or_none = |value: &Option<String>| value.clone().unwrap_or_else(|| "none".into());
        match self {
            Self::Repository { expected, actual } => write!(
                f,
                "repository_id {actual} is not the allowed repository_id {expected}"
            ),
            Self::Ref(git_ref) => write!(f, "ref `{git_ref}` matches none of the allowed refs"),
            Self::Environment { expected, actual } => write!(
                f,
                "environment `{}` is not the required `{expected}`",
                or_none(actual)
            ),
            Self::Workflow { expected, actual } => write!(
                f,
                "job_workflow_ref `{}` is not the required `{expected}`",
                or_none(actual)
            ),
        }
    }
}

impl DeployAuth {
    /// Whether `caller` may deploy the stack: the repository must be the allowed one, and every
    /// optional restriction must hold.
    pub fn check(&self, caller: &DeployCaller) -> Result<(), DeployDenied> {
        if caller.repository_id != self.repository_id {
            return Err(DeployDenied::Repository {
                expected: self.repository_id,
                actual: caller.repository_id,
            });
        }
        if !self.refs.is_empty()
            && !self
                .refs
                .iter()
                .any(|pattern| RefGlob(pattern).matches(&caller.git_ref))
        {
            return Err(DeployDenied::Ref(caller.git_ref.clone()));
        }
        if let Some(expected) = &self.environment
            && caller.environment.as_ref() != Some(expected)
        {
            return Err(DeployDenied::Environment {
                expected: expected.clone(),
                actual: caller.environment.clone(),
            });
        }
        if let Some(expected) = &self.job_workflow_ref
            && caller.job_workflow_ref.as_ref() != Some(expected)
        {
            return Err(DeployDenied::Workflow {
                expected: expected.clone(),
                actual: caller.job_workflow_ref.clone(),
            });
        }
        Ok(())
    }
}

/// A `refs` pattern: `*` matches within one `/`-separated segment, `**` across segments;
/// everything else matches itself.
///
/// The pattern runs as an automaton over the ref, so matching takes time proportional to
/// pattern length times ref length whatever the pattern: no backtracking blow-up on `**/**/…`.
struct RefGlob<'a>(&'a str);

/// One element of a [`RefGlob`].
#[derive(Clone, Copy)]
enum GlobToken {
    Literal(u8),
    /// `*`: any bytes but `/`.
    Segment,
    /// `**`: any bytes.
    Any,
}

impl RefGlob<'_> {
    fn tokens(&self) -> Vec<GlobToken> {
        let mut tokens = Vec::with_capacity(self.0.len());
        let mut bytes = self.0.bytes().peekable();
        while let Some(byte) = bytes.next() {
            tokens.push(match byte {
                b'*' if bytes.next_if_eq(&b'*').is_some() => GlobToken::Any,
                b'*' => GlobToken::Segment,
                literal => GlobToken::Literal(literal),
            });
        }
        tokens
    }

    fn matches(&self, git_ref: &str) -> bool {
        let tokens = self.tokens();
        // `active[i]`: the ref read so far is matched by the first `i` tokens.
        let mut active = vec![false; tokens.len() + 1];
        active[0] = true;
        Self::close(&tokens, &mut active);
        for byte in git_ref.bytes() {
            let mut next = vec![false; tokens.len() + 1];
            for (i, token) in tokens.iter().enumerate() {
                if !active[i] {
                    continue;
                }
                match *token {
                    GlobToken::Literal(literal) if literal == byte => next[i + 1] = true,
                    GlobToken::Literal(_) => {}
                    GlobToken::Segment if byte == b'/' => {}
                    GlobToken::Segment | GlobToken::Any => next[i] = true,
                }
            }
            Self::close(&tokens, &mut next);
            if !next.contains(&true) {
                return false;
            }
            active = next;
        }
        active[tokens.len()]
    }

    /// A wildcard also matches nothing: the state before one reaches the state after it.
    fn close(tokens: &[GlobToken], active: &mut [bool]) {
        for (i, token) in tokens.iter().enumerate() {
            if active[i] && !matches!(token, GlobToken::Literal(_)) {
                active[i + 1] = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> DeployAuth {
        DeployAuth {
            repository_id: 42,
            refs: Vec::new(),
            environment: None,
            job_workflow_ref: None,
        }
    }

    fn caller(git_ref: &str) -> DeployCaller {
        DeployCaller {
            repository_id: 42,
            git_ref: git_ref.into(),
            environment: Some("production".into()),
            job_workflow_ref: Some(
                "acme/notes/.github/workflows/release.yml@refs/heads/main".into(),
            ),
        }
    }

    #[test]
    fn globs_match_within_and_across_segments() {
        let cases = [
            ("refs/tags/v*", "refs/tags/v1.4.2", true),
            ("refs/tags/v*", "refs/tags/v", true),
            ("refs/tags/v*", "refs/tags/release/v1", false),
            ("refs/tags/v*", "refs/heads/v1", false),
            ("refs/heads/*", "refs/heads/feature/x", false),
            ("refs/heads/**", "refs/heads/feature/x", true),
            ("refs/**/v1", "refs/tags/a/b/v1", true),
            ("refs/heads/main", "refs/heads/main", true),
            ("refs/heads/main", "refs/heads/main2", false),
            ("refs/*/main", "refs/heads/main", true),
            ("refs/*/main", "refs/a/b/main", false),
            ("**", "", true),
            ("*", "", true),
            ("", "", true),
            ("", "refs/heads/main", false),
            ("refs/tags/v*.*.*", "refs/tags/v1.4.2", true),
            ("refs/tags/v*.*.*", "refs/tags/v1.4", false),
            ("refs/**", "refs/", true),
            ("refs/**/main", "refs/main", false),
            ("**/main", "refs/heads/main", true),
            ("refs/***", "refs/a/b", true),
        ];
        for (pattern, git_ref, expected) in cases {
            assert_eq!(
                RefGlob(pattern).matches(git_ref),
                expected,
                "{pattern} vs {git_ref}"
            );
        }
    }

    #[test]
    fn many_wildcards_do_not_backtrack_exponentially() {
        let pattern = "**/".repeat(40) + "x";
        let git_ref = "a/".repeat(2000) + "y";
        let started = std::time::Instant::now();
        assert!(!RefGlob(&pattern).matches(&git_ref));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn repository_id_is_always_checked() {
        let other = DeployCaller {
            repository_id: 7,
            ..caller("refs/tags/v1")
        };
        assert_eq!(
            rule().check(&other),
            Err(DeployDenied::Repository {
                expected: 42,
                actual: 7
            })
        );
        assert_eq!(rule().check(&caller("refs/heads/anything")), Ok(()));
    }

    #[test]
    fn optional_restrictions_must_all_hold() {
        let strict = DeployAuth {
            refs: vec!["refs/tags/v*".into()],
            environment: Some("production".into()),
            job_workflow_ref: Some(
                "acme/notes/.github/workflows/release.yml@refs/heads/main".into(),
            ),
            ..rule()
        };
        assert_eq!(strict.check(&caller("refs/tags/v2")), Ok(()));
        assert_eq!(
            strict.check(&caller("refs/heads/main")),
            Err(DeployDenied::Ref("refs/heads/main".into()))
        );
        let no_env = DeployCaller {
            environment: None,
            ..caller("refs/tags/v2")
        };
        assert!(matches!(
            strict.check(&no_env),
            Err(DeployDenied::Environment { .. })
        ));
        let other_workflow = DeployCaller {
            job_workflow_ref: Some("acme/notes/.github/workflows/ci.yml@refs/heads/main".into()),
            ..caller("refs/tags/v2")
        };
        let denied = strict.check(&other_workflow).unwrap_err();
        assert!(matches!(denied, DeployDenied::Workflow { .. }));
        assert!(denied.to_string().contains("ci.yml"), "{denied}");
    }
}
