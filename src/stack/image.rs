//! Image references as written in stack files (ADR-0025).

use std::fmt;
use std::str::FromStr;

/// The registry whose tagless images are built by CI and deployed by digest (ADR-0003).
pub const CI_REGISTRY: &str = "ghcr.io";

/// A content digest, `sha256:<64 lowercase hex>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(String);

impl FromStr for Digest {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.strip_prefix("sha256:") {
            Some(hex)
                if hex.len() == 64
                    && hex
                        .chars()
                        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) =>
            {
                Ok(Self(s.to_owned()))
            }
            _ => Err(format!(
                "invalid digest `{s}`: expected `sha256:` followed by 64 lowercase hex digits"
            )),
        }
    }
}

impl AsRef<str> for Digest {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fully qualified image reference: `registry/repository[:tag][@digest]`.
///
/// The registry must be explicit (`docker.io/rustfs/rustfs`, not `rustfs/rustfs`): rootless
/// podman resolves short names through host configuration, which would make a stack file mean
/// different things on different hosts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageRef {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<Digest>,
}

impl ImageRef {
    pub fn registry(&self) -> &str {
        &self.registry
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    pub fn digest(&self) -> Option<&Digest> {
        self.digest.as_ref()
    }

    /// A CI-built image: on ghcr, without tag or digest. Its digest comes from the webhook or
    /// the poller (ADR-0025, ADR-0043). Everything else is a third-party image.
    pub fn is_ci_built(&self) -> bool {
        self.registry == CI_REGISTRY && self.tag.is_none() && self.digest.is_none()
    }

    /// Whether `tag` is a valid OCI tag: `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`.
    pub fn is_valid_tag(tag: &str) -> bool {
        let mut chars = tag.chars();
        tag.len() <= 128
            && chars
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
    }

    fn is_valid_registry(registry: &str) -> bool {
        let (host, port) = registry
            .split_once(':')
            .map_or((registry, None), |(h, p)| (h, Some(p)));
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
            && port.is_none_or(|p| p.parse::<u16>().is_ok())
    }

    fn is_valid_path_component(component: &str) -> bool {
        let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        component.starts_with(alnum)
            && component.ends_with(alnum)
            && component.chars().all(|c| alnum(c) || "._-".contains(c))
    }
}

impl FromStr for ImageRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let reject = |why: &str| Err(format!("invalid image `{s}`: {why}"));
        let (name, digest) = match s.split_once('@') {
            Some((name, digest)) => (name, Some(digest.parse::<Digest>()?)),
            None => (s, None),
        };
        let last_slash = name.rfind('/').unwrap_or(0);
        let (name, tag) = match name[last_slash..].rfind(':') {
            Some(i) => (&name[..last_slash + i], Some(&name[last_slash + i + 1..])),
            None => (name, None),
        };
        if tag.is_some_and(|tag| !Self::is_valid_tag(tag)) {
            return reject("invalid tag");
        }
        let Some((registry, repository)) = name.split_once('/') else {
            return reject("must name its registry, e.g. `docker.io/library/nginx:1.27`");
        };
        if !(registry.contains('.') || registry.contains(':') || registry == "localhost") {
            return reject("must name its registry, e.g. `docker.io/library/nginx:1.27`");
        }
        if !Self::is_valid_registry(registry) {
            return reject("invalid registry host");
        }
        if !repository.split('/').all(Self::is_valid_path_component) {
            return reject(
                "repository path components must be lowercase letters and digits, \
                 separated by `.`, `_` or `-`",
            );
        }
        Ok(Self {
            registry: registry.to_owned(),
            repository: repository.to_owned(),
            tag: tag.map(str::to_owned),
            digest,
        })
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

impl ImageRef {
    /// This image pinned to `digest`, replacing any digest it already names. CI-built images
    /// get theirs from the webhook or poller, tagged third-party images from the registry client
    /// at stack apply (ADR-0025).
    pub fn pin(self, digest: Digest) -> PinnedImage {
        PinnedImage(Self {
            digest: Some(digest),
            ..self
        })
    }

    /// This image with `tag` instead of its own tag and without a digest: the reference the
    /// poller resolves for a CI-built service that sets `track = "<tag>"` (ADR-0043).
    pub fn with_tag(self, tag: &str) -> Result<Self, String> {
        if !Self::is_valid_tag(tag) {
            return Err(format!("invalid tag `{tag}`"));
        }
        Ok(Self {
            tag: Some(tag.to_owned()),
            digest: None,
            ..self
        })
    }
}

/// An image reference that names its digest: the only kind hoster pulls or runs (ADR-0025).
///
/// Tag resolution happens before this exists, in the registry client at stack apply. A tag the
/// reference also carries is kept for display ([`PinnedImage::tag`]) but plays no part in what
/// is pulled: [`Display`](fmt::Display) prints the pull reference, `registry/repository@digest`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PinnedImage(ImageRef);

impl PinnedImage {
    pub fn registry(&self) -> &str {
        self.0.registry()
    }

    pub fn repository(&self) -> &str {
        self.0.repository()
    }

    /// The tag the stack file named alongside the digest, if any. Informational only.
    pub fn tag(&self) -> Option<&str> {
        self.0.tag()
    }

    pub fn digest(&self) -> &Digest {
        self.0
            .digest
            .as_ref()
            .expect("a PinnedImage always carries a digest")
    }
}

impl TryFrom<ImageRef> for PinnedImage {
    type Error = String;

    fn try_from(image: ImageRef) -> Result<Self, Self::Error> {
        if image.digest.is_none() {
            return Err(format!(
                "image `{image}` is not pinned by digest; resolve its tag first \
                 (expected `registry/repository[:tag]@sha256:…`)"
            ));
        }
        Ok(Self(image))
    }
}

impl FromStr for PinnedImage {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<ImageRef>()?.try_into()
    }
}

impl From<PinnedImage> for ImageRef {
    fn from(image: PinnedImage) -> Self {
        image.0
    }
}

impl AsRef<ImageRef> for PinnedImage {
    fn as_ref(&self) -> &ImageRef {
        &self.0
    }
}

/// The pull reference, `registry/repository@digest`, without the informational tag.
impl fmt::Display for PinnedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}@{}",
            self.registry(),
            self.repository(),
            self.digest()
        )
    }
}

/// How far a newer version tag is from the current one (ADR-0061 §3). Ordered by size, so a
/// policy's limit compares with `<=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl fmt::Display for Bump {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
        })
    }
}

/// An image tag that reads as a version (ADR-0061 §3): an optional `v`, 1–3 dot-separated
/// numeric components, and an optional `-suffix`. `16`, `16.4-alpine`, `v2.3.1` and `24.04`
/// are versions; `latest`, `1.2.3.4`, `2.0.0rc1` and `16-` are not.
///
/// Components are compared as numbers, so `24.04` and `24.4` are the same version; the tag is
/// kept as written for [`Display`](fmt::Display).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TagVersion {
    tag: String,
    v_prefix: bool,
    components: Vec<u64>,
    suffix: Option<String>,
}

/// Words in a suffix that mark a pre-release, matched case-insensitively anywhere in it.
const PRE_RELEASE_MARKERS: [&str; 7] = [
    "rc", "beta", "alpha", "dev", "nightly", "preview", "snapshot",
];

impl TagVersion {
    /// Whether the suffix marks a pre-release (`-rc1`, `-beta.2`, `-alpine-dev`, …), which is
    /// never an update candidate.
    pub fn is_pre_release(&self) -> bool {
        self.suffix.as_deref().is_some_and(|suffix| {
            let suffix = suffix.to_ascii_lowercase();
            PRE_RELEASE_MARKERS
                .iter()
                .any(|marker| suffix.contains(marker))
        })
    }

    /// Whether `other` is in the same version line: the same `v` style, the same number of
    /// components and the same suffix. `16.4-alpine` and `16.6-alpine` are; `16.4-alpine` and
    /// `17.0`, `16.6-bookworm` or `16.4.1-alpine` are not.
    pub fn same_line(&self, other: &Self) -> bool {
        self.v_prefix == other.v_prefix
            && self.components.len() == other.components.len()
            && self.suffix == other.suffix
    }

    /// What kind of update `other` is from `self`: the position of the first component that
    /// grew (major, minor, patch). `None` when `other` is in another line or not newer.
    pub fn bump_kind(&self, other: &Self) -> Option<Bump> {
        if !self.same_line(other) || other.components <= self.components {
            return None;
        }
        let position = self
            .components
            .iter()
            .zip(&other.components)
            .position(|(current, newer)| current != newer)?;
        Some(match position {
            0 => Bump::Major,
            1 => Bump::Minor,
            _ => Bump::Patch,
        })
    }

    /// The highest tag among `tags` that is in `self`'s line, is not a pre-release, and is at
    /// most a `max_bump` away. Tags that are not versions are ignored. `None` when nothing is
    /// newer.
    pub fn newest_in_line<'a>(
        &self,
        tags: impl IntoIterator<Item = &'a str>,
        max_bump: Bump,
    ) -> Option<Self> {
        tags.into_iter()
            .filter_map(|tag| tag.parse::<Self>().ok())
            .filter(|candidate| !candidate.is_pre_release())
            .filter(|candidate| {
                self.bump_kind(candidate)
                    .is_some_and(|bump| bump <= max_bump)
            })
            .max()
    }
}

impl FromStr for TagVersion {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let not_a_version = || {
            format!(
                "tag `{s}` is not a version: expected an optional `v`, one to three numbers \
                 separated by `.`, and an optional `-suffix` (e.g. `16`, `16.4-alpine`, `v2.3.1`)"
            )
        };
        let (v_prefix, rest) = match s.strip_prefix('v') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (numbers, suffix) = match rest.split_once('-') {
            Some((_, "")) => return Err(not_a_version()),
            Some((numbers, suffix)) => (numbers, Some(suffix.to_owned())),
            None => (rest, None),
        };
        let components = numbers
            .split('.')
            .map(|component| {
                if component.is_empty() || !component.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                component.parse::<u64>().ok()
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(not_a_version)?;
        if !(1..=3).contains(&components.len()) {
            return Err(not_a_version());
        }
        Ok(Self {
            tag: s.to_owned(),
            v_prefix,
            components,
            suffix,
        })
    }
}

/// Orders by the numeric components first, so within one line the newest version is the
/// greatest; the rest only makes the order total.
impl Ord for TagVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.components
            .cmp(&other.components)
            .then_with(|| self.v_prefix.cmp(&other.v_prefix))
            .then_with(|| self.suffix.cmp(&other.suffix))
            .then_with(|| self.tag.cmp(&other.tag))
    }
}

impl PartialOrd for TagVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl AsRef<str> for TagVersion {
    fn as_ref(&self) -> &str {
        &self.tag
    }
}

impl fmt::Display for TagVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn tag_and_digest_pins_by_digest() {
        let image: PinnedImage = format!("docker.io/library/nginx:1.27@{DIGEST}")
            .parse()
            .unwrap();
        assert_eq!(image.registry(), "docker.io");
        assert_eq!(image.repository(), "library/nginx");
        assert_eq!(image.tag(), Some("1.27"));
        assert_eq!(image.digest().as_ref(), DIGEST);
        assert_eq!(
            image.to_string(),
            format!("docker.io/library/nginx@{DIGEST}")
        );
        let original = ImageRef::from(image);
        assert_eq!(
            original.to_string(),
            format!("docker.io/library/nginx:1.27@{DIGEST}")
        );
    }

    #[test]
    fn digest_only_pins() {
        let image: PinnedImage = format!("ghcr.io/acme/app@{DIGEST}").parse().unwrap();
        assert_eq!(image.tag(), None);
        assert_eq!(image.to_string(), format!("ghcr.io/acme/app@{DIGEST}"));
        let ported: PinnedImage = format!("localhost:5000/app@{DIGEST}").parse().unwrap();
        assert_eq!(ported.registry(), "localhost:5000");
    }

    #[test]
    fn no_digest_is_rejected() {
        for image in ["docker.io/library/nginx:1.27", "ghcr.io/acme/app"] {
            let err = image.parse::<PinnedImage>().unwrap_err();
            assert!(err.contains("not pinned by digest"), "{image}: {err}");
            let parsed: ImageRef = image.parse().unwrap();
            assert!(PinnedImage::try_from(parsed).is_err());
        }
        // Invalid references are still rejected for their own reasons.
        assert!(
            "docker.io/library/nginx@sha256:00"
                .parse::<PinnedImage>()
                .unwrap_err()
                .contains("invalid digest")
        );
    }

    #[test]
    fn with_tag_replaces_tag_and_drops_digest() {
        let image: ImageRef = format!("ghcr.io/owner/app:v1@{DIGEST}").parse().unwrap();
        let tracked = image.clone().with_tag("main").unwrap();
        assert_eq!(tracked.to_string(), "ghcr.io/owner/app:main");
        assert!(image.with_tag("-bad").is_err());
    }

    #[test]
    fn pin_replaces_any_digest() {
        let other = format!("sha256:{}", "f".repeat(64));
        let image: ImageRef = format!("docker.io/library/nginx:1.27@{DIGEST}")
            .parse()
            .unwrap();
        let pinned = image.pin(other.parse().unwrap());
        assert_eq!(pinned.digest().as_ref(), other);
        assert_eq!(pinned.tag(), Some("1.27"));
        let ci: ImageRef = "ghcr.io/acme/app".parse().unwrap();
        assert_eq!(
            ci.pin(DIGEST.parse().unwrap()).to_string(),
            format!("ghcr.io/acme/app@{DIGEST}")
        );
    }

    fn version(tag: &str) -> TagVersion {
        tag.parse().unwrap_or_else(|e| panic!("{tag}: {e}"))
    }

    #[test]
    fn tag_versions_parse() {
        for (tag, components, v_prefix, suffix) in [
            ("16", &[16][..], false, None),
            ("16.4", &[16, 4][..], false, None),
            ("16.4-alpine", &[16, 4][..], false, Some("alpine")),
            ("16-alpine3.20", &[16][..], false, Some("alpine3.20")),
            ("v2.3.1", &[2, 3, 1][..], true, None),
            ("8.0.1", &[8, 0, 1][..], false, None),
            ("24.04", &[24, 4][..], false, None),
            (
                "1.27.3-bookworm-perl",
                &[1, 27, 3][..],
                false,
                Some("bookworm-perl"),
            ),
        ] {
            let parsed = version(tag);
            assert_eq!(parsed.components, components, "{tag}");
            assert_eq!(parsed.v_prefix, v_prefix, "{tag}");
            assert_eq!(parsed.suffix.as_deref(), suffix, "{tag}");
            assert_eq!(parsed.to_string(), tag);
        }
        for tag in [
            "latest",
            "bookworm",
            "1.2.3.4",
            "2.0.0rc1",
            "16-",
            "v",
            "",
            "1..2",
            ".1",
            "1.",
            "V1.2",
            "-alpine",
            "RELEASE.2025-01-01T00-00-00Z",
            "99999999999999999999999",
        ] {
            let err = tag.parse::<TagVersion>().unwrap_err();
            assert!(err.contains("is not a version"), "{tag}: {err}");
        }
    }

    #[test]
    fn pre_releases_are_flagged() {
        for tag in [
            "2.0.0-rc1",
            "v3.0.0-beta.2",
            "1.0-alpha",
            "8-dev",
            "1-nightly",
            "2-Preview",
            "1.0.0-SNAPSHOT",
        ] {
            assert!(version(tag).is_pre_release(), "{tag}");
        }
        for tag in ["16", "16.4-alpine", "1.27-bookworm"] {
            assert!(!version(tag).is_pre_release(), "{tag}");
        }
    }

    #[test]
    fn lines_need_the_same_prefix_style_component_count_and_suffix() {
        let current = version("16.4-alpine");
        assert!(current.same_line(&version("16.6-alpine")));
        assert!(current.same_line(&version("17.0-alpine")));
        for other in [
            "17.0",
            "16.6-bookworm",
            "16.4.1-alpine",
            "v16.6-alpine",
            "16-alpine",
        ] {
            assert!(!current.same_line(&version(other)), "{other}");
        }
    }

    #[test]
    fn bumps_are_positional() {
        for (from, to, bump) in [
            ("16.4.2", "16.4.3", Some(Bump::Patch)),
            ("16.4.2", "16.5.0", Some(Bump::Minor)),
            ("16.4.2", "17.0.0", Some(Bump::Major)),
            ("16.4", "16.5", Some(Bump::Minor)),
            ("16.4", "17.0", Some(Bump::Major)),
            ("16", "17", Some(Bump::Major)),
            ("v2.3.1", "v2.3.4", Some(Bump::Patch)),
            ("v2.3.1", "v2.4.0", Some(Bump::Minor)),
            ("24.04", "24.10", Some(Bump::Minor)),
            // Not newer, or another line.
            ("16.4.2", "16.4.2", None),
            ("16.4.2", "16.4.1", None),
            ("24.04", "24.4", None),
            ("16.4", "16.4.1", None),
            ("16.4-alpine", "16.5", None),
            ("v2.3.1", "2.3.4", None),
        ] {
            assert_eq!(version(from).bump_kind(&version(to)), bump, "{from} → {to}");
        }
        assert!(Bump::Patch < Bump::Minor && Bump::Minor < Bump::Major);
    }

    #[test]
    fn newest_in_line_picks_the_highest_allowed_candidate() {
        let tags = [
            "latest",
            "16",
            "17",
            "16.3",
            "16.4",
            "16.6",
            "16.10",
            "16.11-alpine",
            "17.0",
            "17.2",
            "18.0-rc1",
            "18.0-beta1",
            "1.2.3.4",
            "16.12-bookworm",
        ];
        let newest = |current: &str, max| {
            version(current)
                .newest_in_line(tags, max)
                .map(|v| v.to_string())
        };
        // Numeric, not lexical: 16.10 beats 16.6.
        assert_eq!(newest("16.4", Bump::Minor).as_deref(), Some("16.10"));
        assert_eq!(newest("16.4", Bump::Major).as_deref(), Some("17.2"));
        // Patch bumps of a 2-component tag do not exist; they arrive as moved digests.
        assert_eq!(newest("16.4", Bump::Patch), None);
        assert_eq!(newest("16", Bump::Major).as_deref(), Some("17"));
        assert_eq!(newest("16", Bump::Minor), None);
        // Pre-releases are skipped even when they are the highest.
        assert_eq!(newest("17.2", Bump::Major), None);
        // Suffixed lines only see their own suffix.
        assert_eq!(
            newest("16.4-alpine", Bump::Minor).as_deref(),
            Some("16.11-alpine")
        );

        let semver = [
            "v2.3.1",
            "v2.3.4",
            "v2.4.0",
            "v2.4.1",
            "v3.0.0",
            "v3.1.0-rc.1",
            "2.9.9",
        ];
        let newest = |current: &str, max| {
            version(current)
                .newest_in_line(semver, max)
                .map(|v| v.to_string())
        };
        assert_eq!(newest("v2.3.1", Bump::Patch).as_deref(), Some("v2.3.4"));
        assert_eq!(newest("v2.3.1", Bump::Minor).as_deref(), Some("v2.4.1"));
        assert_eq!(newest("v2.3.1", Bump::Major).as_deref(), Some("v3.0.0"));
    }

    #[test]
    fn equal_versions_written_differently_tie_deterministically() {
        let picked = version("24.01")
            .newest_in_line(["24.04", "24.4"], Bump::Minor)
            .unwrap();
        // Both are 24.4; the order is total, so the pick is stable.
        assert_eq!(picked, version("24.4").max(version("24.04")));
    }
}
