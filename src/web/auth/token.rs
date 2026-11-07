//! Random tokens for session cookies and CSRF, and their storage hashes.

use std::fmt;

use sha2::{Digest, Sha256};

/// 32 random bytes from the OS, shown as 64 hex digits.
#[derive(Clone, PartialEq, Eq)]
pub struct RandomToken(String);

impl RandomToken {
    const BYTES: usize = 32;

    pub fn generate() -> Self {
        let mut bytes = [0u8; Self::BYTES];
        getrandom::fill(&mut bytes).expect("the OS random number generator works");
        Self(Hex(&bytes).to_string())
    }

    /// The SHA-256 (hex) the store keeps instead of the token itself.
    pub fn hash(value: &str) -> String {
        Hex(&Sha256::digest(value.as_bytes())).to_string()
    }

    /// Compares two secrets in time independent of where they differ.
    pub fn eq_secret(a: &str, b: &str) -> bool {
        let (a, b) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
        a.iter()
            .zip(b.iter())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
    }
}

impl AsRef<str> for RandomToken {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<RandomToken> for String {
    fn from(token: RandomToken) -> String {
        token.0
    }
}

/// Never print a token by accident.
impl fmt::Debug for RandomToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RandomToken(..)")
    }
}

struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (RandomToken::generate(), RandomToken::generate());
        assert_ne!(a, b);
        assert_eq!(a.as_ref().len(), 64);
        assert!(a.as_ref().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(format!("{a:?}"), "RandomToken(..)");
        assert_ne!(RandomToken::hash(a.as_ref()), a.as_ref());
    }

    #[test]
    fn secret_comparison() {
        assert!(RandomToken::eq_secret("abc", "abc"));
        assert!(!RandomToken::eq_secret("abc", "abd"));
        assert!(!RandomToken::eq_secret("abc", "abcd"));
    }
}
