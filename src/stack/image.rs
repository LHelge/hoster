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
}
