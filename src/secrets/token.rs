//! The bearer token that names a service to the OTLP receiver (ADR-0069 §4).
//!
//! A token is `<stack>.<service>.<mac>`: the HMAC-SHA256 of `<stack>.<service>` under a
//! [`TokenKey`], in unpadded URL-safe base64 (43 characters of `[A-Za-z0-9_-]`, so the whole
//! token is safe in an `Authorization` header and needs no escaping in
//! `OTEL_EXPORTER_OTLP_HEADERS`). Names cannot contain `.`, so a token splits in exactly one way.
//!
//! Nothing is stored. The key is derived from the secret store's key with HKDF-SHA256 under a
//! label used for nothing else, so a token reveals nothing usable against stored secrets, and the
//! token is the same for every generation of a service. Changing the secret store's key changes
//! every token; one service's token cannot be rotated on its own.
//!
//! A [`ServiceToken`] parsed from what a client sent is only a claim. [`ServiceToken::verify`]
//! recomputes the MAC, compares it in constant time, and asks [`KnownServices`] whether the
//! stack and service exist now; only then does it hand out the [`ServiceIdentity`] that decides
//! where data is stored.

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;
use thiserror::Error;

use super::SecretKey;
use crate::stack::Name;

type HmacSha256 = Hmac<Sha256>;

/// The HKDF label of the token key. Used for this purpose alone; never reuse it.
const KEY_LABEL: &[u8] = b"hoster otlp service token v1";
/// HMAC-SHA256's output length, in bytes.
const MAC_LEN: usize = 32;

/// Why a presented token was refused. Never carries the token or its MAC.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TokenError {
    /// Not `<stack>.<service>.<mac>` with valid names and a MAC of the right length.
    #[error("malformed service token")]
    Malformed,
    /// The MAC does not match the names under this key.
    #[error("service token does not verify")]
    BadMac,
    /// The token is genuine, but its stack or service does not exist now.
    #[error("service token names {stack}/{service}, which does not exist")]
    Unknown { stack: Name, service: Name },
}

/// The key service tokens are made and verified with, derived from the secret store's key.
/// `Debug` never shows it.
#[derive(Clone)]
pub struct TokenKey([u8; MAC_LEN]);

impl From<&SecretKey> for TokenKey {
    fn from(key: &SecretKey) -> Self {
        let mut okm = [0; MAC_LEN];
        Hkdf::<Sha256>::new(None, &key.0)
            .expand(KEY_LABEL, &mut okm)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self(okm)
    }
}

impl TokenKey {
    /// The token of `service` in `stack`.
    #[allow(dead_code, reason = "injected into containers by task 5up")]
    pub fn token(&self, stack: &Name, service: &Name) -> ServiceToken {
        let mac = self.mac(stack, service).finalize().into_bytes().into();
        ServiceToken {
            stack: stack.clone(),
            service: service.clone(),
            mac,
        }
    }

    /// The MAC over `<stack>.<service>`, not yet finalized.
    fn mac(&self, stack: &Name, service: &Name) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.0).expect("HMAC takes a key of any length");
        mac.update(stack.as_ref().as_bytes());
        mac.update(b".");
        mac.update(service.as_ref().as_bytes());
        mac
    }
}

impl fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenKey(<redacted>)")
    }
}

/// Answers whether a stack and one of its services exist now, from in-memory state: the
/// receiver asks on every request, so no implementation may query the database.
pub trait KnownServices {
    fn contains(&self, stack: &Name, service: &Name) -> bool;
}

/// A service token, made by [`TokenKey::token`] or parsed from what a client sent.
///
/// `Display` writes the bearer value itself: use it only to hand the token to a container, never
/// in a log line. `Debug` shows the names and hides the MAC.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceToken {
    stack: Name,
    service: Name,
    mac: [u8; MAC_LEN],
}

impl ServiceToken {
    /// The service this token is for, if the MAC matches under `key` and the stack and service
    /// exist now. The MAC is compared in constant time.
    pub fn verify(
        self,
        key: &TokenKey,
        known: &(impl KnownServices + ?Sized),
    ) -> Result<ServiceIdentity, TokenError> {
        key.mac(&self.stack, &self.service)
            .verify_slice(&self.mac)
            .map_err(|_| TokenError::BadMac)?;
        if !known.contains(&self.stack, &self.service) {
            return Err(TokenError::Unknown {
                stack: self.stack,
                service: self.service,
            });
        }
        Ok(ServiceIdentity {
            stack: self.stack,
            service: self.service,
        })
    }
}

impl FromStr for ServiceToken {
    type Err = TokenError;

    /// Parses `<stack>.<service>.<mac>`. Anything else, including a MAC of another length or
    /// in non-canonical base64, is [`TokenError::Malformed`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split('.');
        let (Some(stack), Some(service), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenError::Malformed);
        };
        let mut decoded = [0; MAC_LEN];
        match BASE64_URL.decode_slice(mac, &mut decoded) {
            Ok(MAC_LEN) => {}
            _ => return Err(TokenError::Malformed),
        }
        Ok(Self {
            stack: stack.parse().map_err(|_| TokenError::Malformed)?,
            service: service.parse().map_err(|_| TokenError::Malformed)?,
            mac: decoded,
        })
    }
}

impl fmt::Display for ServiceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}",
            self.stack,
            self.service,
            BASE64_URL.encode(self.mac)
        )
    }
}

impl fmt::Debug for ServiceToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceToken")
            .field("stack", &self.stack)
            .field("service", &self.service)
            .field("mac", &format_args!("<redacted>"))
            .finish()
    }
}

/// The stack and service a verified token names: where the receiver stores what it sent,
/// whatever the payload claims.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceIdentity {
    pub stack: Name,
    pub service: Name,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    struct Known(BTreeSet<(Name, Name)>);

    impl KnownServices for Known {
        fn contains(&self, stack: &Name, service: &Name) -> bool {
            self.0.contains(&(stack.clone(), service.clone()))
        }
    }

    fn name(s: &str) -> Name {
        s.parse().unwrap()
    }

    fn key() -> TokenKey {
        TokenKey::from(&SecretKey([7; 32]))
    }

    fn known() -> Known {
        Known(
            [("shop", "web"), ("shop", "worker"), ("blog", "web")]
                .into_iter()
                .map(|(s, v)| (name(s), name(v)))
                .collect(),
        )
    }

    fn token(stack: &str, service: &str) -> ServiceToken {
        key().token(&name(stack), &name(service))
    }

    fn verify(presented: &str) -> Result<ServiceIdentity, TokenError> {
        presented.parse::<ServiceToken>()?.verify(&key(), &known())
    }

    #[test]
    fn round_trip() {
        let text = token("shop", "web").to_string();
        let (prefix, mac) = text.rsplit_once('.').unwrap();
        assert_eq!(prefix, "shop.web");
        assert_eq!(mac.len(), 43, "256-bit MAC in unpadded base64");
        assert!(
            mac.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "header- and URL-safe: {mac}"
        );
        assert_eq!(text.parse::<ServiceToken>().unwrap(), token("shop", "web"));
        assert_eq!(
            verify(&text).unwrap(),
            ServiceIdentity {
                stack: name("shop"),
                service: name("web"),
            }
        );
    }

    #[test]
    fn same_token_every_time() {
        assert_eq!(
            token("shop", "web").to_string(),
            token("shop", "web").to_string()
        );
        assert_ne!(
            token("shop", "web").to_string(),
            token("shop", "worker").to_string()
        );
    }

    #[test]
    fn another_services_mac_fails() {
        let mac = token("shop", "worker").to_string();
        let mac = mac.rsplit_once('.').unwrap().1;
        assert_eq!(verify(&format!("shop.web.{mac}")), Err(TokenError::BadMac));
        let mac = token("blog", "web").to_string();
        let mac = mac.rsplit_once('.').unwrap().1;
        assert_eq!(verify(&format!("shop.web.{mac}")), Err(TokenError::BadMac));
    }

    #[test]
    fn tampered_mac_fails() {
        let mut text = token("shop", "web").to_string().into_bytes();
        // The first MAC character carries six whole bits, so changing it keeps the base64
        // canonical and only changes the MAC.
        let first = "shop.web.".len();
        text[first] = if text[first] == b'A' { b'B' } else { b'A' };
        let text = String::from_utf8(text).unwrap();
        assert_eq!(verify(&text), Err(TokenError::BadMac));
    }

    #[test]
    fn different_key_fails() {
        let other = TokenKey::from(&SecretKey([8; 32]));
        let text = other.token(&name("shop"), &name("web")).to_string();
        assert_ne!(text, token("shop", "web").to_string());
        assert_eq!(verify(&text), Err(TokenError::BadMac));
    }

    #[test]
    fn unknown_stack_or_service_fails() {
        for (stack, service) in [("gone", "web"), ("shop", "gone")] {
            let text = token(stack, service).to_string();
            assert_eq!(
                verify(&text),
                Err(TokenError::Unknown {
                    stack: name(stack),
                    service: name(service),
                })
            );
        }
    }

    #[test]
    fn malformed_input_fails() {
        let good = token("shop", "web").to_string();
        let mac = good.rsplit_once('.').unwrap().1;
        for bad in [
            String::new(),
            "shop".to_owned(),
            "shop.web".to_owned(),
            format!("shop.{mac}"),
            format!("shop.web.{mac}.x"),
            format!(".shop.web.{mac}"),
            format!("shop.web.{mac}="),
            format!("shop.web.{}", &mac[..42]),
            format!("shop.web.{mac}A"),
            format!("Shop.web.{mac}"),
            format!("shop.web_1.{mac}"),
            format!("shop.web.{}", mac.replace(|_| true, "+")),
            format!("shop/web.x.{mac}"),
        ] {
            assert_eq!(verify(&bad), Err(TokenError::Malformed), "{bad:?}");
        }
    }

    #[test]
    fn debug_hides_the_mac_and_key() {
        let token = token("shop", "web");
        let mac = token.to_string().rsplit_once('.').unwrap().1.to_owned();
        let debug = format!("{token:?}");
        assert!(!debug.contains(&mac), "{debug}");
        assert!(
            debug.contains("shop") && debug.contains("<redacted>"),
            "{debug}"
        );
        assert_eq!(format!("{:?}", key()), "TokenKey(<redacted>)");
    }

    #[test]
    fn errors_do_not_echo_the_token() {
        let text = token("shop", "web").to_string();
        let mac = text.rsplit_once('.').unwrap().1;
        for err in [
            verify(&format!("x.{text}")).unwrap_err(),
            verify(&format!("shop.web.{}", &mac[1..])).unwrap_err(),
        ] {
            assert!(!err.to_string().contains(&mac[1..]), "{err}");
        }
    }
}
