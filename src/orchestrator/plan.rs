//! What a deployment changes: per service, the target (digest, config hash) compared with the
//! current pointer (ADR-0005 as refined by ADR-0029). Pure.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::secrets::SecretVersion;
use crate::stack::{Digest, FileSource, Name, PinnedImage, Probe, Service, Stack, UpdateStrategy};
use crate::store::{DigestSet, PortMap, ServicePointer};

/// The current version of each secret a stack references, by secret name. A secret that is not
/// set has no entry; its services hash it as `null` and are rejected in `Validating`.
pub type SecretVersions = BTreeMap<String, SecretVersion>;

/// SHA-256 over a service's resolved spec, everything but the image digest: the second half of
/// the idempotency key (ADR-0029). It covers the container environment including the injected
/// `PORT` (ADR-0051), the *versions* of the secrets it references (never their values),
/// command, volumes, config files (ADR-0057), runtime options and limits (ADR-0059), update
/// strategy, health and routes. It leaves out `depends_on`
/// (ADR-0056 Decision 1): changing it reorders starts and never recreates a container.
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
                // New in ADR-0056; existing specs never hash it, so their hashes stand.
                Probe::Exec { command } => json!({"exec": {"command": command}}),
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
        let mut spec = json!({
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
        // New in ADR-0057 and hashed only when set, so the hashes of services without files
        // stand. Inline content goes in by its digest, a secret by its version; mode and owner
        // with their defaults resolved, so writing a default out changes nothing.
        if !service.files.is_empty() {
            let files: Vec<Value> = service
                .files
                .iter()
                .map(|(path, file)| {
                    let mut entry = json!({
                        "path": path.as_ref(),
                        "mode": file.resolved_mode().to_string(),
                        "owner": file.ownership().to_string(),
                    });
                    match &file.source {
                        FileSource::Inline(content) => {
                            let digest = Sha256::digest(content.as_bytes());
                            entry["sha256"] = Value::from(Self::hex(&digest));
                        }
                        FileSource::Secret(name) => {
                            entry["secret"] = Value::from(name.as_str());
                            entry["version"] =
                                Value::from(secrets.get(name).map(ToString::to_string));
                        }
                    }
                    entry
                })
                .collect();
            spec["files"] = Value::from(files);
        }
        // New in ADR-0059 and hashed only when set, like `files`: sizes as bytes, `tmpfs` as a
        // map sorted by path, `init` only when true.
        if let Some(size) = service.shm_size {
            spec["shm_size"] = Value::from(u64::from(size));
        }
        if !service.tmpfs.is_empty() {
            let tmpfs: BTreeMap<&str, u64> = service
                .tmpfs
                .iter()
                .map(|(path, size)| (path.as_ref(), u64::from(*size)))
                .collect();
            spec["tmpfs"] = json!(tmpfs);
        }
        if service.init {
            spec["init"] = Value::from(true);
        }
        // ADR-0059 §4: each limit only when set (`memory` as bytes, `cpus` as the nano-CPUs
        // podman gets), and an empty table not at all.
        let mut limits = serde_json::Map::new();
        if let Some(memory) = service.limits.memory {
            limits.insert("memory".into(), Value::from(u64::from(memory)));
        }
        if let Some(pids) = service.limits.pids {
            limits.insert("pids".into(), Value::from(pids));
        }
        if let Some(cpus) = service.limits.cpus {
            limits.insert("cpus".into(), Value::from(i64::from(cpus)));
        }
        if !limits.is_empty() {
            spec["limits"] = Value::Object(limits);
        }
        // serde_json's map is ordered, so the text is canonical.
        Self(Self::hex(&Sha256::digest(spec.to_string().as_bytes())))
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
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

/// When a deployment starts a changed service (ADR-0056 Decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Started next to what runs, in `Starting` or `HealthChecking` as soon as its start-phase
    /// dependencies have passed: a start-first service, or a stop-first one without a previous
    /// generation (nothing to stop, no gap).
    Start,
    /// A start-phase service that depends, even transitively, on a replace-phase one: started
    /// and probed in `Replacing` once that dependency has passed, never against the old one.
    HeldBack,
    /// A stop-first service with a previous generation: replaced in `Replacing`, one at a time
    /// in dependency order, behind a 503 gap.
    Replace,
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
    /// When the deployment starts it.
    pub phase: Phase,
    /// The services it names in `depends_on`.
    pub depends_on: BTreeSet<Name>,
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
    /// Every service of the stack, each after its dependencies, name as tie-break.
    order: Vec<Name>,
    /// Each held-back service with the replace-phase services it waits for, even
    /// transitively.
    held_back: BTreeMap<Name, BTreeSet<Name>>,
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
                    phase: Phase::Start,
                    depends_on: service.depends_on.clone(),
                }),
            }
        }
        let replaced: BTreeSet<Name> = plan
            .changes
            .iter()
            .filter(|c| c.update == UpdateStrategy::StopFirst && c.previous.is_some())
            .map(|c| c.service.clone())
            .collect();
        for change in &mut plan.changes {
            let behind: BTreeSet<Name> = stack
                .transitive_dependencies(&change.service)
                .into_iter()
                .filter(|d| replaced.contains(*d))
                .cloned()
                .collect();
            change.phase = if replaced.contains(&change.service) {
                Phase::Replace
            } else if behind.is_empty() {
                Phase::Start
            } else {
                plan.held_back.insert(change.service.clone(), behind);
                Phase::HeldBack
            };
        }
        plan.order = stack.dependency_order().into_iter().cloned().collect();
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

    /// The changes of `phase`, in name order.
    pub fn phase(&self, phase: Phase) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(move |c| c.phase == phase)
    }

    /// What `Replacing` starts, one at a time: the replace-phase and held-back services, in
    /// dependency order with name as tie-break, so every dependency comes first.
    pub fn replacing(&self) -> impl Iterator<Item = &Change> {
        self.order.iter().filter_map(|name| {
            self.changes
                .iter()
                .find(|c| c.service == *name && c.phase != Phase::Start)
        })
    }

    /// The start waves of the start phase (see [`StartWaves`]).
    pub fn start_waves(&self) -> StartWaves {
        let phase: BTreeSet<&Name> = self.phase(Phase::Start).map(|c| &c.service).collect();
        StartWaves {
            waiting: self
                .phase(Phase::Start)
                .map(|c| {
                    let deps = c
                        .depends_on
                        .iter()
                        .filter(|d| phase.contains(d))
                        .cloned()
                        .collect();
                    (c.service.clone(), deps)
                })
                .collect(),
        }
    }

    /// The replace-phase services the held-back `service` waits for, even transitively, in
    /// name order; none for a service that is not held back.
    pub fn held_back_for(&self, service: &Name) -> impl Iterator<Item = &Name> {
        self.held_back.get(service).into_iter().flatten()
    }

    /// The unchanged services `change` names in `depends_on`: what each runs.
    pub fn unchanged_dependencies<'a>(
        &'a self,
        change: &'a Change,
    ) -> impl Iterator<Item = &'a ServicePointer> {
        change
            .depends_on
            .iter()
            .filter_map(|dependency| self.unchanged_pointer(dependency))
    }

    /// The unchanged service `service`, if it is one: what it runs.
    pub fn unchanged_pointer(&self, service: &Name) -> Option<&ServicePointer> {
        self.unchanged
            .iter()
            .find(|p| p.service == service.as_ref())
    }

    /// Whether `service` awaits its first digest: it has never run and is not deployed.
    pub fn is_awaiting(&self, service: &Name) -> bool {
        self.awaiting.contains(service)
    }

    /// The order the deployment starts its changed services in, for the apply preview: the
    /// start phase as levels of its start waves (every service of a level waits only for
    /// services of earlier levels), then `Replacing`'s services one at a time.
    pub fn start_order(&self) -> Vec<Wave> {
        let mut waves = self.start_waves();
        // Each service's start-phase dependencies, before `passed` empties them.
        let mut after = waves.waiting.clone();
        let mut order = Vec::new();
        loop {
            let ready = waves.ready();
            if ready.is_empty() {
                break;
            }
            for service in &ready {
                waves.passed(service);
            }
            order.push(Wave::Start(
                ready
                    .into_iter()
                    .map(|service| {
                        let deps = after.remove(&service).unwrap_or_default();
                        (service, deps.into_iter().collect())
                    })
                    .collect(),
            ));
        }
        order.extend(self.replacing().map(|change| match change.phase {
            Phase::Replace => Wave::Replace(change.service.clone()),
            _ => Wave::HeldBack {
                service: change.service.clone(),
                behind: self.held_back_for(&change.service).cloned().collect(),
            },
        }));
        order
    }
}

/// One step of the order a deployment starts its changed services in (ADR-0056 Decision 2),
/// as [`Plan::start_order`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wave {
    /// Start-phase services started next to what runs, each with the start-phase services it
    /// waits for (all in earlier waves). A service starts as soon as its own dependencies have
    /// passed, never waiting for the rest of the wave before it.
    Start(BTreeMap<Name, Vec<Name>>),
    /// A stop-first service replaced in `Replacing`, behind a 503 gap.
    Replace(Name),
    /// A start-phase service held back until the replaced services it depends on, even
    /// transitively (`behind`), have passed.
    HeldBack { service: Name, behind: Vec<Name> },
}

/// The start phase as start waves (ADR-0056 Decision 2.1): a service is ready once every
/// `depends_on` service in the start phase has passed. Services ready together start together,
/// and one that is ready never waits for an unrelated one, so these are not strict waves.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StartWaves {
    /// Start-phase services not started yet, with their start-phase dependencies that have
    /// not passed.
    waiting: BTreeMap<Name, BTreeSet<Name>>,
}

impl StartWaves {
    /// The services that can start now, in name order; they are no longer waiting.
    pub fn ready(&mut self) -> Vec<Name> {
        let ready: Vec<Name> = self
            .waiting
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(name, _)| name.clone())
            .collect();
        for name in &ready {
            self.waiting.remove(name);
        }
        ready
    }

    /// `service` passed its check: its dependents no longer wait for it.
    pub fn passed(&mut self, service: &Name) {
        for deps in self.waiting.values_mut() {
            deps.remove(service);
        }
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

    /// ADR-0057 §7: a file's content, mode, owner and secret version are all part of its
    /// service's hash, and of no other's.
    #[test]
    fn config_hash_covers_config_files() {
        let none = SecretVersions::new();
        let with_files = |files: &str| {
            stack(&STACK.replace(
                "image = \"ghcr.io/acme/notes-api\"",
                &format!("image = \"ghcr.io/acme/notes-api\"\nfiles = {{ {files} }}"),
            ))
        };
        let plain = stack(STACK);
        let base = with_files(r#""/etc/a.conf" = { content = "a = 1" }"#);
        assert_ne!(hash(&plain, "api", &none), hash(&base, "api", &none));
        assert_eq!(
            hash(&plain, "storage", &none),
            hash(&base, "storage", &none)
        );

        for changed in [
            r#""/etc/a.conf" = { content = "a = 2" }"#,
            r#""/etc/b.conf" = { content = "a = 1" }"#,
            r#""/etc/a.conf" = { content = "a = 1", mode = "0400" }"#,
            r#""/etc/a.conf" = { content = "a = 1", owner = "1000:1000" }"#,
            r#""/etc/a.conf" = { content = "a = 1" }, "/etc/b.conf" = { content = "" }"#,
            r#""/etc/a.conf" = { secret = "notes/a" }"#,
        ] {
            let other = with_files(changed);
            assert_ne!(
                hash(&base, "api", &none),
                hash(&other, "api", &none),
                "{changed}"
            );
            assert_eq!(
                hash(&base, "storage", &none),
                hash(&other, "storage", &none)
            );
        }
        // Writing the defaults out is the same file.
        let explicit =
            with_files(r#""/etc/a.conf" = { content = "a = 1", mode = "0444", owner = "0:0" }"#);
        assert_eq!(hash(&base, "api", &none), hash(&explicit, "api", &none));

        // A secret file follows its secret's version, like an env secret.
        let secret = with_files(r#""/run/secrets/db" = { secret = "notes/db" }"#);
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
        let unset = hash(&secret, "api", &none);
        let v1 = hash(&secret, "api", &version(1));
        assert_ne!(unset, v1);
        assert_ne!(v1, hash(&secret, "api", &version(2)));
        assert_eq!(v1, hash(&secret, "api", &version(1)));
        assert_eq!(
            hash(&secret, "storage", &none),
            hash(&secret, "storage", &version(1))
        );
    }

    /// ADR-0059 §4: `shm_size`, `tmpfs` and `init` are part of the hash when set, by bytes, and
    /// `tmpfs = {}` and `init = false` hash like a service without them.
    #[test]
    fn config_hash_covers_runtime_options_only_when_set() {
        let none = SecretVersions::new();
        let with = |lines: &str| {
            stack(&STACK.replace(
                "image = \"ghcr.io/acme/notes-api\"",
                &format!("image = \"ghcr.io/acme/notes-api\"\n{lines}"),
            ))
        };
        let plain = hash(&stack(STACK), "api", &none);
        for unset in ["tmpfs = {}", "init = false", "tmpfs = {}\ninit = false"] {
            assert_eq!(plain, hash(&with(unset), "api", &none), "{unset}");
        }
        let base = with("shm_size = \"256MiB\"\ntmpfs = { \"/tmp\" = \"64MiB\" }\ninit = true");
        assert_ne!(plain, hash(&base, "api", &none));
        assert_eq!(
            hash(&stack(STACK), "storage", &none),
            hash(&base, "storage", &none)
        );
        // The same bytes in another unit are the same spec.
        let respelled =
            with("shm_size = \"268435456B\"\ntmpfs = { \"/tmp\" = \"65536KiB\" }\ninit = true");
        assert_eq!(hash(&base, "api", &none), hash(&respelled, "api", &none));
        let mut hashes = std::collections::HashSet::from([hash(&base, "api", &none)]);
        for changed in [
            "shm_size = \"256MiB\"",
            "tmpfs = { \"/tmp\" = \"64MiB\" }",
            "init = true",
            "shm_size = \"256MB\"\ntmpfs = { \"/tmp\" = \"64MiB\" }\ninit = true",
            "shm_size = \"256MiB\"\ntmpfs = { \"/tmp\" = \"65MiB\" }\ninit = true",
            "shm_size = \"256MiB\"\ntmpfs = { \"/run\" = \"64MiB\" }\ninit = true",
            "shm_size = \"256MiB\"\ntmpfs = { \"/tmp\" = \"64MiB\", \"/run\" = \"1MiB\" }\ninit = true",
        ] {
            assert!(
                hashes.insert(hash(&with(changed), "api", &none)),
                "{changed} hashes like another spec"
            );
        }
    }

    /// ADR-0059 §4: each limit is part of the hash only when set, `memory` by bytes and `cpus`
    /// by nano-CPUs; `limits = {}` hashes like a service without the table.
    #[test]
    fn config_hash_covers_limits_only_when_set() {
        let none = SecretVersions::new();
        let with = |limits: &str| {
            stack(&STACK.replace(
                "image = \"ghcr.io/acme/notes-api\"",
                &format!("image = \"ghcr.io/acme/notes-api\"\nlimits = {{ {limits} }}"),
            ))
        };
        let plain = hash(&stack(STACK), "api", &none);
        assert_eq!(plain, hash(&with(""), "api", &none));
        let base = with("memory = \"512MiB\", pids = 256, cpus = 1.5");
        assert_ne!(plain, hash(&base, "api", &none));
        assert_eq!(
            hash(&stack(STACK), "storage", &none),
            hash(&base, "storage", &none)
        );
        // The same bytes or nano-CPUs written another way are the same spec.
        for same in [
            "memory = \"536870912B\", pids = 256, cpus = 1.5",
            "memory = \"524288KiB\", pids = 256, cpus = 1.5000000001",
        ] {
            assert_eq!(
                hash(&base, "api", &none),
                hash(&with(same), "api", &none),
                "{same}"
            );
        }
        let mut hashes = std::collections::HashSet::from([hash(&base, "api", &none), plain]);
        for changed in [
            "memory = \"512MiB\"",
            "pids = 256",
            "cpus = 1.5",
            "memory = \"512MB\", pids = 256, cpus = 1.5",
            "memory = \"512MiB\", pids = 257, cpus = 1.5",
            "memory = \"512MiB\", pids = 256, cpus = 1.6",
            "memory = \"512MiB\", pids = 256, cpus = 1.500000001",
        ] {
            assert!(
                hashes.insert(hash(&with(changed), "api", &none)),
                "{changed} hashes like another spec"
            );
        }
    }

    /// `depends_on` reorders starts but never recreates a container (ADR-0056 Decision 1); an
    /// exec check is part of the spec.
    #[test]
    fn config_hash_skips_depends_on_but_covers_exec() {
        let none = SecretVersions::new();
        let base = stack(STACK);
        let dependent = stack(&STACK.replace(
            "image = \"ghcr.io/acme/notes-api\"",
            "image = \"ghcr.io/acme/notes-api\"\ndepends_on = [\"storage\"]",
        ));
        assert_eq!(hash(&base, "api", &none), hash(&dependent, "api", &none));

        let exec = |argv: &str| {
            stack(&format!(
                "{STACK}\n[services.storage.health]\nexec = {argv}\n"
            ))
        };
        let ready = hash(&exec("[\"ready\"]"), "storage", &none);
        assert_ne!(hash(&base, "storage", &none), ready);
        assert_ne!(ready, hash(&exec("[\"ready\", \"-q\"]"), "storage", &none));
        assert_eq!(ready, hash(&exec("[\"ready\"]"), "storage", &none));
    }

    /// The phases of ADR-0056 Decision 2: a stop-first service without a previous generation
    /// starts with the start-first ones; one with a previous generation is replaced; a
    /// start-phase service depending on it, even through an unchanged one, is held back.
    #[test]
    fn changes_are_classified_into_phases() {
        const PHASES: &str = r#"
version = 1
name = "shop"

[services.app]
image = "docker.io/acme/app:1"
depends_on = ["cache"]

[services.cache]
image = "docker.io/library/redis:7"
depends_on = ["db"]

[services.db]
image = "docker.io/library/postgres:17"
update = "stop-first"

[services.search]
image = "docker.io/library/meili:1"
update = "stop-first"

[services.web]
image = "docker.io/acme/web:1"
depends_on = ["search"]
"#;
        let s = stack(PHASES);
        let pins: DigestSet = ["app", "cache", "db", "search", "web"]
            .into_iter()
            .map(|n| (n.to_owned(), digest(1)))
            .collect();
        let none = SecretVersions::new();
        // First deploy: everything is in the start phase, dependencies first.
        let plan = Plan::new(&s, &pins, &DigestSet::new(), &[], &none).unwrap();
        assert!(plan.changes.iter().all(|c| c.phase == Phase::Start));
        let mut waves = plan.start_waves();
        let ready = waves.ready();
        assert_eq!(
            ready,
            ["db".parse::<Name>().unwrap(), "search".parse().unwrap()]
        );
        waves.passed(&"search".parse().unwrap());
        assert_eq!(waves.ready(), ["web".parse::<Name>().unwrap()]);
        waves.passed(&"db".parse().unwrap());
        assert_eq!(waves.ready(), ["cache".parse::<Name>().unwrap()]);
        waves.passed(&"cache".parse().unwrap());
        assert_eq!(waves.ready(), ["app".parse::<Name>().unwrap()]);
        assert!(waves.ready().is_empty());

        // db and app change, cache does not: app is held back behind db through cache.
        let running: Vec<ServicePointer> = ["app", "cache", "db", "search", "web"]
            .into_iter()
            .map(|n| pointer(&s, n, 1, 1))
            .collect();
        let requested: DigestSet = [("app".into(), digest(2)), ("db".into(), digest(2))].into();
        let plan = Plan::new(&s, &pins, &requested, &running, &none).unwrap();
        let phases: Vec<(&str, Phase)> = plan
            .changes
            .iter()
            .map(|c| (c.service.as_ref(), c.phase))
            .collect();
        assert_eq!(phases, [("app", Phase::HeldBack), ("db", Phase::Replace)]);
        let behind: Vec<&Name> = plan.held_back_for(&"app".parse().unwrap()).collect();
        assert_eq!(behind, [&"db".parse::<Name>().unwrap()]);
        assert_eq!(plan.held_back_for(&"db".parse().unwrap()).count(), 0);
        let replacing: Vec<&str> = plan.replacing().map(|c| c.service.as_ref()).collect();
        assert_eq!(replacing, ["db", "app"]);
        assert!(plan.start_waves().ready().is_empty());
        assert!(plan.unchanged_pointer(&"cache".parse().unwrap()).is_some());
    }

    /// The apply preview's start order: start waves by level, then `Replacing` one at a time.
    #[test]
    fn start_order_lists_waves_then_replacements() {
        const ORDER: &str = r#"
version = 1
name = "shop"

[services.app]
image = "docker.io/acme/app:1"
depends_on = ["cache", "db"]

[services.cache]
image = "docker.io/library/redis:7"

[services.db]
image = "docker.io/library/postgres:17"
update = "stop-first"

[services.worker]
image = "docker.io/acme/worker:1"
depends_on = ["app"]
"#;
        let name = |n: &str| n.parse::<Name>().unwrap();
        let s = stack(ORDER);
        let pins: DigestSet = ["app", "cache", "db", "worker"]
            .into_iter()
            .map(|n| (n.to_owned(), digest(1)))
            .collect();
        let none = SecretVersions::new();
        // First deploy: db has no previous generation, so it starts in the start phase.
        let plan = Plan::new(&s, &pins, &DigestSet::new(), &[], &none).unwrap();
        assert_eq!(
            plan.start_order(),
            [
                Wave::Start(BTreeMap::from([
                    (name("cache"), Vec::new()),
                    (name("db"), Vec::new()),
                ])),
                Wave::Start(BTreeMap::from([(
                    name("app"),
                    vec![name("cache"), name("db")]
                )])),
                Wave::Start(BTreeMap::from([(name("worker"), vec![name("app")])])),
            ]
        );

        // Everything changes again: db is replaced, app and worker wait behind it.
        let running: Vec<ServicePointer> = ["app", "cache", "db", "worker"]
            .into_iter()
            .map(|n| pointer(&s, n, 2, 1))
            .collect();
        let plan = Plan::new(&s, &pins, &DigestSet::new(), &running, &none).unwrap();
        assert_eq!(
            plan.start_order(),
            [
                Wave::Start(BTreeMap::from([(name("cache"), Vec::new())])),
                Wave::Replace(name("db")),
                Wave::HeldBack {
                    service: name("app"),
                    behind: vec![name("db")],
                },
                Wave::HeldBack {
                    service: name("worker"),
                    behind: vec![name("db")],
                },
            ]
        );
        // Nothing changes: no order.
        let running: Vec<ServicePointer> = ["app", "cache", "db", "worker"]
            .into_iter()
            .map(|n| pointer(&s, n, 1, 1))
            .collect();
        let plan = Plan::new(&s, &pins, &DigestSet::new(), &running, &none).unwrap();
        assert!(plan.start_order().is_empty());

        // Only app changes: its dependencies run unchanged, so it waits for nothing in the
        // start phase. A removed service is not started at all.
        let mut running: Vec<ServicePointer> = ["cache", "db", "worker"]
            .into_iter()
            .map(|n| pointer(&s, n, 1, 1))
            .chain([pointer(&s, "app", 2, 1)])
            .collect();
        running.push(ServicePointer {
            service: "gone".into(),
            ..pointer(&s, "cache", 1, 1)
        });
        let plan = Plan::new(&s, &pins, &DigestSet::new(), &running, &none).unwrap();
        assert_eq!(plan.removed.len(), 1);
        assert_eq!(
            plan.start_order(),
            [Wave::Start(BTreeMap::from([(name("app"), Vec::new())]))]
        );
    }

    /// Hashes recorded before `health.exec` and `depends_on` existed (ADR-0056): adding them
    /// must not recreate any running container.
    #[test]
    fn config_hash_is_stable_for_existing_specs() {
        let none = SecretVersions::new();
        let s = stack(&format!(
            "{STACK}\n[services.api.health]\nhttp = {{ path = \"/api/healthz\" }}\nstart_period = \"1m\"\n\n[[services.storage.routes]]\nhost = \"s3.example.com\"\nport = 9000\n\n[services.storage.health]\ntcp = {{ port = 9000 }}\n"
        ));
        let hashes = [
            hash(&stack(STACK), "api", &none).to_string(),
            hash(&stack(STACK), "storage", &none).to_string(),
            hash(&s, "api", &none).to_string(),
            hash(&s, "storage", &none).to_string(),
        ];
        assert_eq!(
            hashes,
            [
                "08cd366b6cfd1fac82df5fff38fc10f5265faa655f1cf9714e77b4e1728acacb",
                "e77545e8b51f5194bee9dce401c2cafdaa064c2893d6c2985f29e8bd905c93af",
                "89bc79f29dd74ae38632e12d5a7de45f0c4a85e016fd4b5c0a456ae88ccb6cf4",
                "cdd26fd4b5dc8e035961957016b2d79abe397481273f1bfb9d8b83e41375a1f8",
            ]
        );
    }
}
