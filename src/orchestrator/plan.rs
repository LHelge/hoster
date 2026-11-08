//! What a deployment changes: per service, the target (digest, config hash) compared with the
//! current pointer (ADR-0005 as refined by ADR-0029). Pure.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::secrets::SecretVersion;
use crate::stack::{Digest, Name, PinnedImage, Probe, Service, Stack, UpdateStrategy};
use crate::store::{DigestSet, PortMap, ServicePointer};

/// The current version of each secret a stack references, by secret name. A secret that is not
/// set has no entry; its services hash it as `null` and are rejected in `Validating`.
pub type SecretVersions = BTreeMap<String, SecretVersion>;

/// SHA-256 over a service's resolved spec, everything but the image digest: the second half of
/// the idempotency key (ADR-0029). It covers the container environment including the injected
/// `PORT` (ADR-0051), the *versions* of the secrets it references (never their values),
/// command, volumes, update strategy, health and routes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConfigHash(String);

impl ConfigHash {
    /// The hash of `service` with the secret versions `secrets`.
    pub fn new(service: &Service, secrets: &SecretVersions) -> Self {
        let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let opt_millis = |d: Option<Duration>| d.map(millis);
        let health = service.health.as_ref().map(|h| {
            let probe = match &h.probe {
                Probe::Http {
                    path,
                    port,
                    expect_status,
                } => json!({"http": {"path": path, "port": port,
                                     "expect": format!("{expect_status:?}")}}),
                Probe::Tcp { port } => json!({"tcp": {"port": port}}),
            };
            json!({
                "probe": probe,
                "interval": opt_millis(h.interval),
                "timeout": opt_millis(h.timeout),
                "healthy_after": h.healthy_after,
                "start_period": opt_millis(h.start_period),
            })
        });
        let routes: Vec<Value> = service
            .routes
            .iter()
            .map(|r| {
                json!({
                    "host": r.host.to_string(),
                    "prefix": r.prefix.to_string(),
                    "strip_prefix": r.strip_prefix,
                    "port": r.port,
                    "timeouts": [
                        opt_millis(r.timeouts.connect),
                        opt_millis(r.timeouts.read),
                        opt_millis(r.timeouts.write),
                        opt_millis(r.timeouts.idle),
                    ],
                })
            })
            .collect();
        let secret_refs: BTreeMap<&str, Value> = service
            .secrets
            .iter()
            .map(|(env, name)| {
                let version = secrets.get(name).map(ToString::to_string);
                (env.as_str(), json!({"name": name, "version": version}))
            })
            .collect();
        let spec = json!({
            "image": format!("{}/{}", service.image.registry(), service.image.repository()),
            "command": service.command,
            "entrypoint": service.entrypoint,
            "env": service.container_env(),
            "secrets": secret_refs,
            "volumes": service.volumes,
            "health": health,
            "update": format!("{:?}", service.update),
            "stop_timeout": millis(service.stop_timeout),
            "user": service.user,
            "read_only": service.read_only,
            "routes": routes,
        });
        // serde_json's map is ordered, so the text is canonical.
        let digest = Sha256::digest(spec.to_string().as_bytes());
        Self(digest.iter().map(|b| format!("{b:02x}")).collect())
    }
}

impl AsRef<str> for ConfigHash {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConfigHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One service a deployment recreates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub service: Name,
    pub image: PinnedImage,
    pub config: ConfigHash,
    pub update: UpdateStrategy,
    /// What runs now; `None` for a service that never ran.
    pub previous: Option<ServicePointer>,
}

impl Change {
    /// The distinct container ports the service's routes use: one loopback port each.
    pub fn container_ports(&self, stack: &Stack) -> BTreeSet<u16> {
        stack.services[&self.service]
            .routes
            .iter()
            .map(|r| r.port)
            .collect()
    }

    /// The pointer this change commits.
    pub fn pointer(&self, generation: u32, ports: PortMap) -> ServicePointer {
        ServicePointer {
            service: self.service.to_string(),
            generation,
            digest: self.image.digest().to_string(),
            config_hash: self.config.to_string(),
            ports,
        }
    }
}

/// Why no plan could be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("stack has no service {0:?}")]
    UnknownService(String),
    #[error("service {service}: {message}")]
    Digest { service: String, message: String },
}

/// The comparison of a deployment's target with the stack's current pointers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Services to recreate, in name order.
    pub changes: Vec<Change>,
    /// Services that keep their container and generation.
    pub unchanged: Vec<ServicePointer>,
    /// Services that run but are no longer in the stack: removed at clean-up.
    pub removed: Vec<ServicePointer>,
    /// CI-built services that have never received a digest (no webhook yet).
    pub awaiting: Vec<Name>,
}

impl Plan {
    /// Compares the target of a deployment of `stack` with `running`.
    ///
    /// Each service's digest is the `requested` one if given, else the revision's pinned digest
    /// (third-party images, ADR-0025), else the one it runs now (CI-built images keep theirs);
    /// a CI-built service with none of these is awaiting its first digest.
    pub fn new(
        stack: &Stack,
        revision_images: &DigestSet,
        requested: &DigestSet,
        running: &[ServicePointer],
        secrets: &SecretVersions,
    ) -> Result<Self, PlanError> {
        if let Some(unknown) = requested
            .keys()
            .find(|s| !stack.services.keys().any(|n| n.as_ref() == s.as_str()))
        {
            return Err(PlanError::UnknownService(unknown.clone()));
        }
        let running: BTreeMap<&str, &ServicePointer> =
            running.iter().map(|p| (p.service.as_str(), p)).collect();
        let mut plan = Plan::default();
        for (name, service) in &stack.services {
            let current = running.get(name.as_ref()).copied();
            let digest = requested
                .get(name.as_ref())
                .or_else(|| revision_images.get(name.as_ref()))
                .cloned()
                .or_else(|| service.image.digest().map(ToString::to_string))
                .or_else(|| current.map(|p| p.digest.clone()));
            let Some(digest) = digest else {
                plan.awaiting.push(name.clone());
                continue;
            };
            let digest: Digest = digest.parse().map_err(|message| PlanError::Digest {
                service: name.to_string(),
                message,
            })?;
            let config = ConfigHash::new(service, secrets);
            match current {
                Some(p) if p.digest == digest.as_ref() && p.config_hash == config.as_ref() => {
                    plan.unchanged.push(p.clone());
                }
                _ => plan.changes.push(Change {
                    service: name.clone(),
                    image: service.image.clone().pin(digest),
                    config,
                    update: service.update,
                    previous: current.cloned(),
                }),
            }
        }
        plan.removed = running
            .values()
            .filter(|p| !stack.services.keys().any(|n| n.as_ref() == p.service))
            .map(|p| (*p).clone())
            .collect();
        Ok(plan)
    }

    /// Whether the deployment would change nothing.
    pub fn is_noop(&self) -> bool {
        self.changes.is_empty() && self.removed.is_empty()
    }

    /// The full digest set the stack runs once the deployment is live.
    pub fn target(&self) -> DigestSet {
        self.changes
            .iter()
            .map(|c| (c.service.to_string(), c.image.digest().to_string()))
            .chain(
                self.unchanged
                    .iter()
                    .map(|p| (p.service.clone(), p.digest.clone())),
            )
            .collect()
    }

    /// The changes with `update` strategy, in name order.
    pub fn changes_with(&self, update: UpdateStrategy) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(move |c| c.update == update)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STACK: &str = r#"
version = 1
name = "notes"

[services.api]
image = "ghcr.io/acme/notes-api"

[[services.api.routes]]
host = "notes.example.com"
prefix = "/api"
port = 8080

[services.storage]
image = "docker.io/rustfs/rustfs:1.0.0"
update = "stop-first"
"#;

    fn digest(n: u8) -> String {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
    }

    fn stack(source: &str) -> Stack {
        source.parse().unwrap()
    }

    fn pointer(stack: &Stack, service: &str, d: u8, generation: u32) -> ServicePointer {
        let name: Name = service.parse().unwrap();
        ServicePointer {
            service: service.into(),
            generation,
            digest: digest(d),
            config_hash: ConfigHash::new(&stack.services[&name], &SecretVersions::new())
                .to_string(),
            ports: PortMap::new(),
        }
    }

    fn pins() -> DigestSet {
        DigestSet::from([("storage".into(), digest(9))])
    }

    #[test]
    fn first_deploy_changes_every_service_with_a_digest() {
        let s = stack(STACK);
        let requested = DigestSet::from([("api".into(), digest(1))]);
        let plan = Plan::new(&s, &pins(), &requested, &[], &SecretVersions::new()).unwrap();
        let names: Vec<_> = plan.changes.iter().map(|c| c.service.to_string()).collect();
        assert_eq!(names, ["api", "storage"]);
        assert!(plan.awaiting.is_empty());
        assert_eq!(plan.target().len(), 2);
    }

    #[test]
    fn ci_service_without_digest_awaits_its_first() {
        let s = stack(STACK);
        let plan = Plan::new(&s, &pins(), &DigestSet::new(), &[], &SecretVersions::new()).unwrap();
        assert_eq!(plan.awaiting, ["api".parse::<Name>().unwrap()]);
        assert_eq!(plan.changes.len(), 1);
    }

    #[test]
    fn same_digest_and_config_is_a_noop() {
        let s = stack(STACK);
        let running = [pointer(&s, "api", 1, 3), pointer(&s, "storage", 9, 2)];
        let requested = DigestSet::from([("api".into(), digest(1))]);
        let plan = Plan::new(&s, &pins(), &requested, &running, &SecretVersions::new()).unwrap();
        assert!(plan.is_noop());
        // Without a requested digest, CI-built services keep theirs.
        assert!(
            Plan::new(
                &s,
                &pins(),
                &DigestSet::new(),
                &running,
                &SecretVersions::new()
            )
            .unwrap()
            .is_noop()
        );
    }

    #[test]
    fn a_new_digest_changes_only_that_service() {
        let s = stack(STACK);
        let running = [pointer(&s, "api", 1, 3), pointer(&s, "storage", 9, 2)];
        let requested = DigestSet::from([("api".into(), digest(2))]);
        let plan = Plan::new(&s, &pins(), &requested, &running, &SecretVersions::new()).unwrap();
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].service.as_ref(), "api");
        assert_eq!(plan.changes[0].previous.as_ref().unwrap().generation, 3);
        assert_eq!(plan.unchanged[0].generation, 2);
    }

    #[test]
    fn a_config_change_changes_the_service() {
        let s = stack(STACK);
        let running = [pointer(&s, "api", 1, 3), pointer(&s, "storage", 9, 2)];
        let edited = stack(&STACK.replace(
            "update = \"stop-first\"",
            "update = \"stop-first\"\nenv = { A = \"1\" }",
        ));
        let plan = Plan::new(
            &edited,
            &pins(),
            &DigestSet::new(),
            &running,
            &SecretVersions::new(),
        )
        .unwrap();
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].service.as_ref(), "storage");
        assert_eq!(plan.changes[0].update, UpdateStrategy::StopFirst);
    }

    #[test]
    fn services_dropped_from_the_stack_are_removed() {
        let s = stack(STACK);
        let mut running = vec![pointer(&s, "api", 1, 3), pointer(&s, "storage", 9, 2)];
        running.push(ServicePointer {
            service: "old".into(),
            ..running[0].clone()
        });
        let plan = Plan::new(
            &s,
            &pins(),
            &DigestSet::new(),
            &running,
            &SecretVersions::new(),
        )
        .unwrap();
        assert!(!plan.is_noop());
        assert_eq!(plan.removed[0].service, "old");
    }

    #[test]
    fn unknown_requested_service_is_an_error() {
        let s = stack(STACK);
        let requested = DigestSet::from([("nope".into(), digest(1))]);
        assert_eq!(
            Plan::new(&s, &pins(), &requested, &[], &SecretVersions::new()),
            Err(PlanError::UnknownService("nope".into()))
        );
    }

    fn hash(stack: &Stack, service: &str, secrets: &SecretVersions) -> ConfigHash {
        ConfigHash::new(&stack.services[&service.parse::<Name>().unwrap()], secrets)
    }

    #[test]
    fn config_hash_ignores_the_digest_but_not_the_spec() {
        let none = SecretVersions::new();
        let a = stack(STACK);
        let b = stack(STACK);
        assert_eq!(hash(&a, "api", &none), hash(&b, "api", &none));
        let c = stack(&STACK.replace("port = 8080", "port = 8081"));
        assert_ne!(hash(&a, "api", &none), hash(&c, "api", &none));
        assert_eq!(hash(&a, "api", &none).as_ref().len(), 64);
    }

    #[test]
    fn config_hash_covers_the_injected_port() {
        let none = SecretVersions::new();
        let injected = stack(STACK);
        // Setting `PORT` explicitly to the injected value is the same container environment.
        let explicit = stack(&STACK.replace(
            "image = \"ghcr.io/acme/notes-api\"",
            "image = \"ghcr.io/acme/notes-api\"\nenv = { PORT = \"8080\" }",
        ));
        assert_eq!(hash(&injected, "api", &none), hash(&explicit, "api", &none));
        let other = stack(&STACK.replace(
            "image = \"ghcr.io/acme/notes-api\"",
            "image = \"ghcr.io/acme/notes-api\"\nenv = { PORT = \"9999\" }",
        ));
        assert_ne!(hash(&injected, "api", &none), hash(&other, "api", &none));
    }

    #[test]
    fn config_hash_follows_secret_versions_not_just_names() {
        let s = stack(&STACK.replace(
            "image = \"ghcr.io/acme/notes-api\"",
            "image = \"ghcr.io/acme/notes-api\"\nsecrets = { DB = \"notes/db\" }",
        ));
        let version = |n: u8| {
            let sealed = crate::store::SealedSecret {
                name: "notes/db".into(),
                nonce: vec![n; 24],
                ciphertext: Vec::new(),
                created_at: crate::store::Timestamp::now(),
                updated_at: crate::store::Timestamp::now(),
            };
            SecretVersions::from([("notes/db".into(), SecretVersion::from(&sealed))])
        };
        let unset = hash(&s, "api", &SecretVersions::new());
        let v1 = hash(&s, "api", &version(1));
        let v2 = hash(&s, "api", &version(2));
        assert_ne!(unset, v1);
        assert_ne!(v1, v2);
        assert_eq!(v1, hash(&s, "api", &version(1)));
        // Other services do not reference the secret.
        assert_eq!(
            hash(&s, "storage", &SecretVersions::new()),
            hash(&s, "storage", &version(1))
        );

        // A replaced secret changes only the services that use it.
        let running = [
            ServicePointer {
                config_hash: v1.to_string(),
                ..pointer(&s, "api", 1, 3)
            },
            pointer(&s, "storage", 9, 2),
        ];
        let plan = Plan::new(&s, &pins(), &DigestSet::new(), &running, &version(1)).unwrap();
        assert!(plan.is_noop());
        let plan = Plan::new(&s, &pins(), &DigestSet::new(), &running, &version(2)).unwrap();
        let names: Vec<_> = plan.changes.iter().map(|c| c.service.to_string()).collect();
        assert_eq!(names, ["api"]);
    }
}
