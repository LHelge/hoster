//! The `OTEL_*` variables hoster gives every service (ADR-0069 §5), so that an app with an
//! OpenTelemetry SDK that reads its environment exports to the receiver without being told.
//!
//! Four are part of the service's environment as `PORT` is (ADR-0051), and so of its config
//! hash: [`ExporterEnv::hashed`]. Two are added only when a container is created, and are not
//! hashed: the bearer header ([`ExporterEnv::headers`], a secret) and the resource attributes
//! ([`ResourceAttributes`]), which carry the deployment generation. A stack file that sets one
//! of these names as `env` or as a secret keeps its own; that rule is applied where each is
//! added, per variable.

use std::fmt;

use super::OtlpReceiver;
use crate::secrets::{SecretValue, ServiceToken};
use crate::stack::Name;

/// The exporter settings of one service, as the environment variables of the OpenTelemetry
/// SDK specification.
pub struct ExporterEnv;

impl ExporterEnv {
    /// Where the SDK sends: the receiver on the proxy's `:80`, as containers reach the host.
    pub const ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
    /// OTLP/HTTP with protobuf bodies, the only encoding the receiver takes (ADR-0069 §2).
    pub const PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
    /// `none`: the receiver takes no metrics (ADR-0069 §2).
    pub const METRICS_EXPORTER: &str = "OTEL_METRICS_EXPORTER";
    /// The service's name, for display; the token decides where data is stored.
    pub const SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
    /// `Authorization=Bearer%20<token>`: the service's token (ADR-0069 §4). A secret.
    pub const HEADERS: &str = "OTEL_EXPORTER_OTLP_HEADERS";
    /// The stack and the deployment generation ([`ResourceAttributes`]).
    pub const RESOURCE_ATTRIBUTES: &str = "OTEL_RESOURCE_ATTRIBUTES";

    /// The variables that are the same for every deployment of `service`, in the order the
    /// docs list them: part of its environment and its config hash.
    pub fn hashed(service: &Name) -> [(&'static str, String); 4] {
        [
            (Self::ENDPOINT, OtlpReceiver::ENDPOINT.to_owned()),
            (Self::PROTOCOL, "http/protobuf".to_owned()),
            (Self::METRICS_EXPORTER, "none".to_owned()),
            (Self::SERVICE_NAME, service.to_string()),
        ]
    }

    /// The value of [`Self::HEADERS`] for `token`. The header list is W3C-baggage-like, so the
    /// space after `Bearer` is percent-encoded; the token itself needs no escaping.
    pub fn headers(token: &ServiceToken) -> SecretValue {
        SecretValue::from(format!("Authorization=Bearer%20{token}"))
    }

    /// What the environment view shows for [`Self::HEADERS`]: the shape of the value with the
    /// token masked, as a secret's value is never shown (ADR-0029).
    pub const HEADERS_MASKED: &str = "Authorization=Bearer%20••••••••";
}

/// The resource attributes hoster gives a container: its stack and its deployment generation.
/// `Display` writes the value of `OTEL_RESOURCE_ATTRIBUTES`. The receiver reads the generation
/// back from what the SDK sends ([`ResourceAttributes::generation`]), for display only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceAttributes<'a> {
    pub stack: &'a Name,
    pub generation: u32,
}

impl ResourceAttributes<'_> {
    /// The stack, under OpenTelemetry's semantic convention for a group of services.
    pub const NAMESPACE: &str = "service.namespace";
    /// The deployment generation (ADR-0017), hoster's own attribute.
    pub const GENERATION: &str = "hoster.generation";

    /// The generation an SDK sent as the [`Self::GENERATION`] attribute, given as text (as the
    /// environment variable makes it) or as an integer. `None` when absent or not a `u32`.
    pub fn generation(value: &serde_json::Value) -> Option<u32> {
        match value {
            serde_json::Value::String(text) => text.trim().parse().ok(),
            serde_json::Value::Number(number) => {
                number.as_u64().and_then(|n| u32::try_from(n).ok())
            }
            _ => None,
        }
    }
}

/// `service.namespace=<stack>,hoster.generation=<generation>`. Stack names are lowercase
/// letters, digits and `-`, so nothing needs percent-encoding.
impl fmt::Display for ResourceAttributes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}={},{}={}",
            Self::NAMESPACE,
            self.stack,
            Self::GENERATION,
            self.generation
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::secrets::{SecretKey, TokenKey};

    fn name(s: &str) -> Name {
        s.parse().unwrap()
    }

    #[test]
    fn the_hashed_variables_point_the_sdk_at_the_receiver() {
        let vars = ExporterEnv::hashed(&name("web"));
        assert_eq!(
            vars.map(|(k, v)| format!("{k}={v}")),
            [
                "OTEL_EXPORTER_OTLP_ENDPOINT=http://host.containers.internal",
                "OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf",
                "OTEL_METRICS_EXPORTER=none",
                "OTEL_SERVICE_NAME=web",
            ]
        );
    }

    #[test]
    fn the_header_carries_the_token_and_nothing_prints_it() {
        let key = TokenKey::from(&SecretKey::generate());
        let token = key.token(&name("shop"), &name("web"));
        let header = ExporterEnv::headers(&token);
        assert_eq!(header.expose(), format!("Authorization=Bearer%20{token}"));
        assert_eq!(format!("{header:?}"), "<redacted>");
        assert!(!ExporterEnv::HEADERS_MASKED.contains(&token.to_string()));
    }

    #[test]
    fn resource_attributes_carry_the_stack_and_generation_both_ways() {
        let stack = name("shop");
        let attributes = ResourceAttributes {
            stack: &stack,
            generation: 7,
        };
        assert_eq!(
            attributes.to_string(),
            "service.namespace=shop,hoster.generation=7"
        );
        assert_eq!(ResourceAttributes::generation(&json!("7")), Some(7));
        assert_eq!(ResourceAttributes::generation(&json!(7)), Some(7));
        for wrong in [
            json!("x"),
            json!(-1),
            json!(1.5),
            json!(null),
            json!(u64::MAX),
        ] {
            assert_eq!(ResourceAttributes::generation(&wrong), None, "{wrong}");
        }
    }
}
