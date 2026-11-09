//! Zero-downtime deploy scenarios (ADR-0017, ADR-0048, architecture §5): real health checks
//! through a fake prober, the `RouteTable` swap, the in-flight drain and crash recovery, with
//! the orchestrator against [`FakeRuntime`] and a real (temporary) store.
//!
//! A "crash" freezes the deploy task in a state (a held pull, a probe that never answers, a
//! request that never finishes) and starts a fresh orchestrator on the same store and podman,
//! as hoster does after a restart: [`Orchestrator::recover`], then
//! [`Orchestrator::restore_routes`].

use std::sync::Arc;
use std::time::Duration;

use super::tests::{FakeProber, Harness, NOTES, digest, harness, harness_with, settings};
use super::*;
use crate::config::PortRange;
use crate::routing::{Target, Upstream};
use crate::runtime::{ContainerState, ExecOutcome, FakeRuntime, Operation};

use DeploymentState as S;

/// NOTES with probes on api (HTTP) and storage (TCP) that give up after 200 ms.
fn probed() -> String {
    NOTES
        .replace(
            "[services.web]",
            "[services.api.health]\nhttp = { path = \"/healthz\", port = 8080 }\n\
             interval = \"10ms\"\nstart_period = \"200ms\"\n\n[services.web]",
        )
        .replace(
            "[[services.storage.routes]]",
            "[services.storage.health]\ntcp = { port = 9000 }\n\
             interval = \"10ms\"\nstart_period = \"200ms\"\n\n[[services.storage.routes]]",
        )
}

/// `toml` with a stack-wide `drain_timeout`.
fn with_drain_timeout(toml: &str, timeout: &str) -> String {
    toml.replace(
        "name = \"notes\"",
        &format!("name = \"notes\"\ndrain_timeout = \"{timeout}\""),
    )
}

/// `toml` with storage's config changed, so a deploy replaces it (stop-first).
fn storage_changed(toml: &str) -> String {
    toml.replace(
        "data = \"/data\"",
        "data = \"/data\"\n\n[services.storage.env]\nA = \"1\"",
    )
}

/// `toml` with api's and storage's config changed: one deployment replaces both.
fn both_changed(toml: &str) -> String {
    storage_changed(toml).replace(
        "[services.web]",
        "[services.api.env]\nB = \"1\"\n\n[services.web]",
    )
}

/// The upstream the table sends `host` + `path` to.
fn upstream(routes: &ArcSwap<RouteTable>, host: &str, path: &str) -> Option<Arc<Upstream>> {
    match &routes.load().lookup(host, path)?.target {
        Target::Loopback(upstream) => Some(Arc::clone(upstream)),
        _ => None,
    }
}

fn api(routes: &ArcSwap<RouteTable>) -> Arc<Upstream> {
    upstream(routes, "notes.example.com", "/api/notes").expect("api is routed")
}

fn storage(routes: &ArcSwap<RouteTable>) -> Arc<Upstream> {
    upstream(routes, "s3.notes.example.com", "/bucket").expect("storage is routed")
}

/// Applies `toml` and deploys api and web: every service live (api g2, web g2, storage g1).
async fn bootstrap_with(h: &Harness, toml: &str) {
    let applied = h.apply(toml).await;
    let first = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(first.state, S::Live, "{first:?}");
    let second = h.finish(&h.deploy(&[("api", 1), ("web", 1)]).await).await;
    assert_eq!(second.state, S::Live, "{second:?}");
}

impl Harness {
    /// Waits until deployment `id` is in `state`.
    async fn reaches(&self, id: DeploymentId, state: DeploymentState) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let deployment = self.store.deployment(id).await.unwrap().unwrap();
                if deployment.state == state {
                    return;
                }
                assert!(
                    !deployment.state.is_terminal(),
                    "ended in {} before reaching {state}",
                    deployment.state
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("deployment never reached {state}"));
    }

    /// hoster after a restart: a fresh orchestrator, with a fresh table, on the same store and
    /// podman, recovered and with its routes restored.
    async fn restart(&self) -> (Arc<Orchestrator<FakeRuntime>>, Arc<ArcSwap<RouteTable>>) {
        let routes = Arc::<ArcSwap<RouteTable>>::default();
        let orchestrator = Orchestrator::with_prober(
            self.store.clone(),
            Arc::clone(&self.runtime),
            Arc::clone(&self.resolver) as Arc<dyn TagResolver>,
            PortAllocator::new(
                self.store.clone(),
                PortRange::try_from(20000..=20015).unwrap(),
            ),
            self.secrets.clone(),
            settings(Duration::from_millis(20)),
            Arc::clone(&routes),
            Arc::new(FakeProber::default()),
        );
        orchestrator.recover().await.unwrap();
        orchestrator.restore_routes().await.unwrap();
        (orchestrator, routes)
    }

    async fn api_port(&self) -> u16 {
        let pointers = self.store.services("notes").await.unwrap();
        pointers.iter().find(|p| p.service == "api").unwrap().ports[&8080]
    }
}

#[tokio::test]
async fn the_new_generation_runs_next_to_the_old_one_until_the_swap() {
    let h = harness_with(Duration::from_millis(200)).await;
    h.bootstrap().await;
    let old = api(&h.routes);
    assert_eq!(old.generation, 2);
    let outcome = h.deploy(&[("api", 2)]).await;
    h.reaches(outcome.deployment, S::HealthChecking).await;

    // Both generations run, the new one on a fresh port; the table still routes the old one.
    let g2 = h.container("hoster-notes-api-g2");
    let g3 = h.container("hoster-notes-api-g3");
    assert!(g2.state.is_running() && g3.state.is_running());
    let new_port = g3.ports[0].host_port;
    assert_ne!(new_port, old.addr.port());
    assert!(Arc::ptr_eq(&api(&h.routes), &old));

    let live = h.finish(&outcome).await;
    assert_eq!(live.state, S::Live);
    // The orchestrator's publish moved the route; the pointer agrees; the old port is free.
    let new = api(&h.routes);
    assert_eq!((new.generation, new.addr.port()), (3, new_port));
    assert_eq!(h.api_port().await, new_port);
    assert!(
        !h.container_names()
            .contains(&"hoster-notes-api-g2".to_owned())
    );
    let reserved: Vec<u16> = h
        .store
        .port_allocations()
        .await
        .unwrap()
        .iter()
        .map(|a| a.port)
        .collect();
    assert!(!reserved.contains(&old.addr.port()), "{reserved:?}");
    // The deployment probed the new container with the default check (ADR-0051).
    let probes = h.prober.take_probes();
    assert!(
        probes
            .iter()
            .any(|p| p == &format!("GET /healthz on container port 8080 (127.0.0.1:{new_port})")),
        "{probes:?}"
    );
    assert!(
        h.notes(live.id)
            .await
            .iter()
            .any(|(state, note)| *state == S::HealthChecking && note.contains("passed")),
    );
}

#[tokio::test]
async fn draining_waits_for_requests_in_flight_on_the_old_upstream() {
    let h = harness().await;
    h.bootstrap().await;
    let old = api(&h.routes);
    let request = old.track();
    let outcome = h.deploy(&[("api", 2)]).await;
    h.reaches(outcome.deployment, S::Draining).await;

    // Traffic already goes to g3, but g2 keeps running for the request still in flight.
    assert_eq!(api(&h.routes).generation, 3);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let deployment = h
        .store
        .deployment(outcome.deployment)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deployment.state, S::Draining);
    assert!(deployment.committed_at.is_some());
    assert!(h.container("hoster-notes-api-g2").state.is_running());

    drop(request);
    let live = h.finish(&outcome).await;
    assert_eq!(live.state, S::Live);
    assert!(
        !h.container_names()
            .contains(&"hoster-notes-api-g2".to_owned())
    );
    assert!(
        h.notes(live.id)
            .await
            .iter()
            .all(|(_, n)| !n.contains("timed out"))
    );
}

#[tokio::test]
async fn the_drain_timeout_bounds_long_requests() {
    let h = harness().await;
    bootstrap_with(&h, &with_drain_timeout(NOTES, "50ms")).await;
    let _stuck = api(&h.routes).track();
    let started = tokio::time::Instant::now();
    let live = h.finish(&h.deploy(&[("api", 2)]).await).await;
    assert_eq!(live.state, S::Live);
    assert!(started.elapsed() < Duration::from_secs(5));
    let notes = h.notes(live.id).await;
    assert!(
        notes.iter().any(|(state, note)| *state == S::Draining
            && note.contains("drain of service api g2")
            && note.contains("timed out after 50ms with 1 request(s) in flight")),
        "{notes:?}"
    );
    assert!(
        !h.container_names()
            .contains(&"hoster-notes-api-g2".to_owned())
    );
}

#[tokio::test]
async fn removing_the_stack_cuts_the_drain_short() {
    let h = harness().await;
    bootstrap_with(&h, &with_drain_timeout(NOTES, "1h")).await;
    let _stuck = api(&h.routes).track();
    let outcome = h.deploy(&[("api", 2)]).await;
    h.reaches(outcome.deployment, S::Draining).await;

    // The removal does not sit through the drain timeout: it takes every container down.
    let removed = tokio::time::timeout(
        Duration::from_secs(5),
        h.orchestrator.remove(RemoveStack::keeping_volumes("notes")),
    )
    .await
    .expect("the removal does not wait for the drain")
    .unwrap();
    assert_eq!(removed.containers, 3);
    assert!(h.runtime.containers().is_empty());
    let ended = h
        .store
        .deployment(outcome.deployment)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended.state, S::Live);
}

#[tokio::test]
async fn a_failing_probe_aborts_and_the_old_generation_keeps_serving() {
    let h = harness().await;
    bootstrap_with(&h, &probed()).await;
    let before = h.container_names();
    let old = api(&h.routes);
    let version = h.routes.load().version();
    h.prober.answer(8080, Err("HTTP 503, expected 2xx"));

    let failed = h.finish(&h.deploy(&[("api", 2)]).await).await;
    assert_eq!(failed.state, S::Failed);
    let error = failed.error.as_deref().unwrap();
    assert!(error.contains("did not pass within 200ms"), "{error}");
    assert!(error.contains("HTTP 503"), "{error}");
    assert_eq!(
        h.states(failed.id).await,
        [
            S::Received,
            S::Validating,
            S::Pulling,
            S::Starting,
            S::HealthChecking,
            S::Aborting,
            S::Failed
        ]
    );
    assert_eq!(h.container_names(), before);
    // Nothing was published: the old upstream serves, with its pointer and port.
    assert_eq!(h.routes.load().version(), version);
    assert!(Arc::ptr_eq(&api(&h.routes), &old));
    assert_eq!(h.api_port().await, old.addr.port());
    assert_eq!(h.store.port_allocations().await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_failing_start_first_service_never_stops_a_stop_first_one() {
    let h = harness().await;
    bootstrap_with(&h, &probed()).await;
    h.prober.answer(8080, Err("HTTP 500, expected 2xx"));
    // One deployment changes api (start-first) and storage (stop-first).
    let applied = h.apply(&both_changed(&probed())).await;
    assert_eq!(applied.changed, ["api", "storage"]);
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed);
    assert!(!h.states(failed.id).await.contains(&S::Replacing));
    // storage was never stopped: no outage for a broken api image (ADR-0048).
    let calls = h.runtime.calls();
    assert!(!calls.contains(&Operation::Stop), "{calls:?}");
    assert_eq!(
        h.container("hoster-notes-storage-g1").state,
        ContainerState::Running
    );
    assert_eq!(storage(&h.routes).generation, 1);
}

#[tokio::test]
async fn a_failing_stop_first_service_aborts_the_whole_stack() {
    let h = harness().await;
    bootstrap_with(&h, &probed()).await;
    let old_api = api(&h.routes);
    let old_storage = storage(&h.routes);
    h.prober
        .answer(9000, Err("cannot connect: connection refused"));
    let applied = h.apply(&both_changed(&probed())).await;
    assert_eq!(applied.changed, ["api", "storage"]);
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed);
    assert_eq!(
        h.states(failed.id).await,
        [
            S::Received,
            S::Validating,
            S::Pulling,
            S::Starting,
            S::HealthChecking,
            S::Replacing,
            S::Aborting,
            S::Failed
        ]
    );
    // api passed its probe but was never swapped in; the stack is exactly as it was.
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g2",
            "hoster-notes-storage-g1",
            "hoster-notes-web-g2"
        ]
    );
    assert_eq!(
        h.container("hoster-notes-storage-g1").state,
        ContainerState::Running
    );
    assert!(Arc::ptr_eq(&api(&h.routes), &old_api));
    assert!(Arc::ptr_eq(&storage(&h.routes), &old_storage));
    assert_eq!(
        h.generations().await,
        [("api".into(), 2), ("storage".into(), 1), ("web".into(), 2)]
    );
    h.assert_one_generation_each(&h.routes).await;
}

#[tokio::test]
async fn a_mixed_deployment_swaps_every_service_in_one_publish() {
    let h = harness_with(Duration::from_millis(200)).await;
    let before = NOTES.replace(
        "[services.web]",
        "[services.api.env]\nB = \"0\"\n\n[services.web]",
    );
    bootstrap_with(&h, &before).await;
    h.prober.take_probes();
    let applied = h.apply(&both_changed(NOTES)).await;
    assert_eq!(applied.changed, ["api", "storage"]);
    let id = applied.deployment.as_ref().unwrap().deployment;
    // While storage is replaced, api's new generation is probed and healthy but not routed yet.
    h.reaches(id, S::Replacing).await;
    assert!(
        h.prober
            .take_probes()
            .iter()
            .any(|p| p.contains("container port 8080"))
    );
    assert_eq!(api(&h.routes).generation, 2);
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live);
    assert_eq!(api(&h.routes).generation, 3);
    assert_eq!(storage(&h.routes).generation, 3);
    assert_eq!(
        h.generations().await,
        [("api".into(), 3), ("storage".into(), 3), ("web".into(), 2)]
    );
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g3",
            "hoster-notes-storage-g3",
            "hoster-notes-web-g2"
        ]
    );
    h.assert_one_generation_each(&h.routes).await;
}

/// `probed()` with a second stop-first service, `db` (TCP probe on 5432, its own host), which
/// sorts before storage and so is replaced first.
fn two_stop_first() -> String {
    probed().replace(
        "[services.storage]",
        "[services.db]\nimage = \"docker.io/library/postgres:17\"\nupdate = \"stop-first\"\n\n\
         [services.db.volumes]\npg = \"/var/lib/postgresql\"\n\n\
         [services.db.health]\ntcp = { port = 5432 }\ninterval = \"10ms\"\nstart_period = \"200ms\"\n\n\
         [[services.db.routes]]\nhost = \"db.notes.example.com\"\nport = 5432\n\n\
         [services.storage]",
    )
}

/// `toml` with db's and storage's config changed: one deployment replaces both stop-first
/// services.
fn both_stop_first_changed(toml: &str) -> String {
    storage_changed(toml).replace(
        "pg = \"/var/lib/postgresql\"",
        "pg = \"/var/lib/postgresql\"\n\n[services.db.env]\nC = \"1\"",
    )
}

fn db(routes: &ArcSwap<RouteTable>) -> Arc<Upstream> {
    upstream(routes, "db.notes.example.com", "/").expect("db is routed")
}

impl Harness {
    /// Every service of the stack runs exactly one container, of the generation its pointer
    /// names, and every routed service is served by that generation.
    async fn assert_one_generation_each(&self, routes: &ArcSwap<RouteTable>) {
        let containers = self.runtime.containers();
        for pointer in self.store.services("notes").await.unwrap() {
            let prefix = format!("hoster-notes-{}-g", pointer.service);
            let own: Vec<_> = containers
                .iter()
                .filter(|c| c.name.starts_with(&prefix))
                .collect();
            assert_eq!(own.len(), 1, "{}: {own:?}", pointer.service);
            assert_eq!(
                own[0].name,
                format!("{prefix}{}", pointer.generation),
                "{}",
                pointer.service
            );
            assert!(own[0].state.is_running(), "{} not running", own[0].name);
            if let Some(&host_port) = pointer.ports.values().next() {
                let (host, path) = match pointer.service.as_str() {
                    "api" => ("notes.example.com", "/api/notes"),
                    "web" => ("notes.example.com", "/"),
                    "storage" => ("s3.notes.example.com", "/bucket"),
                    "db" => ("db.notes.example.com", "/"),
                    other => panic!("no known route for {other}"),
                };
                let served = upstream(routes, host, path)
                    .unwrap_or_else(|| panic!("{} is not served", pointer.service));
                assert_eq!(
                    (served.generation, served.addr.port()),
                    (pointer.generation, host_port),
                    "{}",
                    pointer.service
                );
            }
        }
    }
}

#[tokio::test]
async fn a_stop_first_service_is_replaced_behind_its_gap() {
    let h = harness().await;
    h.bootstrap().await;
    let old = storage(&h.routes);
    h.prober.take_probes();
    // The new container's probe does not answer until the gap has been looked at.
    h.prober.hold(9000);
    let applied = h.apply(&storage_changed(NOTES)).await;
    assert_eq!(applied.changed, ["storage"]);
    let id = applied.deployment.as_ref().unwrap().deployment;

    // The new container is probed while the old one is stopped, kept, and its routes answer 503.
    h.reaches(id, S::Replacing).await;
    h.prober.probed(9000).await;
    assert!(matches!(
        h.routes
            .load()
            .lookup("s3.notes.example.com", "/bucket")
            .unwrap()
            .target,
        Target::Unavailable { .. }
    ));
    let g1 = h.container("hoster-notes-storage-g1");
    assert!(!g1.state.is_running(), "{g1:?}");
    assert!(h.container("hoster-notes-storage-g3").state.is_running());

    // The old container is removed in CleaningUp, after the swap.
    h.prober.release(9000);
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
    assert_eq!(
        h.states(live.id).await,
        [
            S::Received,
            S::Validating,
            S::Pulling,
            S::Starting,
            S::HealthChecking,
            S::Replacing,
            S::Switching,
            S::Draining,
            S::CleaningUp,
            S::Live
        ]
    );
    let new = storage(&h.routes);
    assert_eq!(new.generation, 3);
    assert!(!Arc::ptr_eq(&old, &new));
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g2",
            "hoster-notes-storage-g3",
            "hoster-notes-web-g2"
        ]
    );
    h.assert_one_generation_each(&h.routes).await;
}

#[tokio::test]
async fn a_start_first_only_deployment_passes_through_replacing_without_effects() {
    let h = harness().await;
    h.bootstrap().await;
    let old_storage = storage(&h.routes);
    let calls = h.runtime.calls().len();
    let live = h.finish(&h.deploy(&[("api", 2)]).await).await;
    assert_eq!(live.state, S::Live);
    assert!(h.states(live.id).await.contains(&S::Replacing));
    // The stop-first service was never touched; the only stop is api's old generation in
    // CleaningUp.
    assert!(Arc::ptr_eq(&storage(&h.routes), &old_storage));
    let stops = h.runtime.calls()[calls..]
        .iter()
        .filter(|c| **c == Operation::Stop)
        .count();
    assert_eq!(stops, 1);
    h.assert_one_generation_each(&h.routes).await;
}

#[tokio::test]
async fn a_second_failing_stop_first_service_restarts_the_first_one() {
    let h = harness().await;
    bootstrap_with(&h, &two_stop_first()).await;
    let old_db = db(&h.routes);
    let old_storage = storage(&h.routes);
    let before = h.container_names();
    assert_eq!(
        before,
        [
            "hoster-notes-api-g2",
            "hoster-notes-db-g1",
            "hoster-notes-storage-g1",
            "hoster-notes-web-g2"
        ]
    );
    h.prober
        .answer(9000, Err("cannot connect: connection refused"));

    let applied = h.apply(&both_stop_first_changed(&two_stop_first())).await;
    assert_eq!(applied.changed, ["db", "storage"]);
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    assert!(h.states(failed.id).await.contains(&S::Replacing));
    // db was replaced (stopped, new one started and healthy) before storage failed.
    let probes = h.prober.take_probes();
    assert!(
        probes.iter().any(|p| p.contains("container port 5432")),
        "{probes:?}"
    );
    let calls = h.runtime.calls();
    assert!(
        calls.iter().filter(|c| **c == Operation::Stop).count() >= 2,
        "{calls:?}"
    );

    // Both old containers run again, both new ones are gone, and the old upstreams are back.
    assert_eq!(h.container_names(), before);
    assert!(Arc::ptr_eq(&db(&h.routes), &old_db));
    assert!(Arc::ptr_eq(&storage(&h.routes), &old_storage));
    assert_eq!(
        h.generations().await,
        [
            ("api".into(), 2),
            ("db".into(), 1),
            ("storage".into(), 1),
            ("web".into(), 2)
        ]
    );
    h.assert_one_generation_each(&h.routes).await;
    assert_eq!(h.store.port_allocations().await.unwrap().len(), 4);
}

#[tokio::test]
async fn the_first_stop_first_service_stays_in_its_gap_until_the_commit() {
    let h = harness().await;
    bootstrap_with(&h, &two_stop_first()).await;
    h.prober.take_probes();
    h.prober.hold(9000);
    let applied = h.apply(&both_stop_first_changed(&two_stop_first())).await;
    let id = applied.deployment.as_ref().unwrap().deployment;
    h.reaches(id, S::Replacing).await;
    h.prober.probed(9000).await;

    // db's new generation is healthy but not routed before the single swap; both are in a gap.
    let table = h.routes.load();
    for host in ["db.notes.example.com", "s3.notes.example.com"] {
        assert!(
            matches!(
                table.lookup(host, "/").unwrap().target,
                Target::Unavailable { .. }
            ),
            "{host}"
        );
    }
    assert!(h.container("hoster-notes-db-g3").state.is_running());
    assert!(!h.container("hoster-notes-db-g1").state.is_running());
    assert!(!h.container("hoster-notes-storage-g1").state.is_running());

    // storage passes; the commit routes both new generations at once.
    h.prober.release(9000);
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
    assert_eq!(
        (db(&h.routes).generation, storage(&h.routes).generation),
        (3, 3)
    );
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g2",
            "hoster-notes-db-g3",
            "hoster-notes-storage-g3",
            "hoster-notes-web-g2"
        ]
    );
    h.assert_one_generation_each(&h.routes).await;
}

// Crash at each state, then restart (architecture §5.4).

/// After a crash before the commit: the old generation serves, the new one is gone.
async fn assert_rolled_back(h: &Harness, id: DeploymentId, routes: &ArcSwap<RouteTable>) {
    let deployment = h.store.deployment(id).await.unwrap().unwrap();
    assert_eq!(deployment.state, S::Failed);
    assert_eq!(
        deployment.error.as_deref(),
        Some("interrupted by a hoster restart")
    );
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g2",
            "hoster-notes-storage-g1",
            "hoster-notes-web-g2"
        ]
    );
    for name in h.container_names() {
        assert!(h.container(&name).state.is_running(), "{name}");
    }
    assert_eq!(api(routes).generation, 2);
    assert_eq!(api(routes).addr.port(), h.api_port().await);
    assert_eq!(storage(routes).generation, 1);
    assert_eq!(h.store.port_allocations().await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_crash_while_pulling_keeps_the_old_generation() {
    let h = harness().await;
    h.bootstrap().await;
    let hold = h.runtime.hold_pulls("notes-api");
    let outcome = h.deploy(&[("api", 2)]).await;
    hold.reached().await;
    let (_, routes) = h.restart().await;
    assert_rolled_back(&h, outcome.deployment, &routes).await;
}

#[tokio::test]
async fn a_crash_while_health_checking_removes_the_new_generation() {
    let h = harness().await;
    h.bootstrap().await;
    h.prober.hold(8080);
    let outcome = h.deploy(&[("api", 2)]).await;
    h.reaches(outcome.deployment, S::HealthChecking).await;
    h.prober.probed(8080).await;
    assert!(
        h.container_names()
            .contains(&"hoster-notes-api-g3".to_owned())
    );
    let (_, routes) = h.restart().await;
    assert_rolled_back(&h, outcome.deployment, &routes).await;
}

#[tokio::test]
async fn a_crash_while_replacing_restarts_the_stopped_stop_first_container() {
    let h = harness().await;
    h.bootstrap().await;
    h.prober.hold(9000);
    let applied = h.apply(&storage_changed(NOTES)).await;
    let id = applied.deployment.as_ref().unwrap().deployment;
    h.reaches(id, S::Replacing).await;
    h.prober.probed(9000).await;
    assert_ne!(
        h.container("hoster-notes-storage-g1").state,
        ContainerState::Running,
        "Replacing stopped the old storage container"
    );
    let (_, routes) = h.restart().await;
    assert_rolled_back(&h, id, &routes).await;
}

#[tokio::test]
async fn a_crash_while_draining_keeps_the_committed_generation() {
    let h = harness().await;
    h.bootstrap().await;
    let _stuck = api(&h.routes).track();
    let outcome = h.deploy(&[("api", 2)]).await;
    h.reaches(outcome.deployment, S::Draining).await;
    let new_port = h.api_port().await;

    let (_, routes) = h.restart().await;
    let deployment = h
        .store
        .deployment(outcome.deployment)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deployment.state, S::Live);
    // The committed pointers are authoritative: g3 is routed, g2 and its port are gone.
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g3",
            "hoster-notes-storage-g1",
            "hoster-notes-web-g2"
        ]
    );
    let api = api(&routes);
    assert_eq!((api.generation, api.addr.port()), (3, new_port));
    let api_ports: Vec<_> = h
        .store
        .port_allocations()
        .await
        .unwrap()
        .into_iter()
        .filter(|a| a.service == "api")
        .map(|a| a.port)
        .collect();
    assert_eq!(api_ports, [new_port]);
}

#[tokio::test]
async fn after_a_crash_the_next_deployment_runs_normally() {
    let h = harness().await;
    h.bootstrap().await;
    h.prober.hold(8080);
    let outcome = h.deploy(&[("api", 2)]).await;
    h.reaches(outcome.deployment, S::HealthChecking).await;
    let (orchestrator, routes) = h.restart().await;
    let again = orchestrator
        .deploy(
            "notes",
            [("api".to_owned(), digest(2))].into(),
            DeploymentSource::Webhook,
        )
        .await
        .unwrap();
    let live = tokio::time::timeout(Duration::from_secs(10), orchestrator.wait(again.deployment))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.state, S::Live);
    assert_eq!(api(&routes).generation, live.generation.unwrap());
    assert_eq!(api(&routes).addr.port(), h.api_port().await);
}

// Service dependencies (ADR-0056 Decision 2).

/// `two_stop_first()` with api depending on the stop-first db.
fn api_on_db() -> String {
    two_stop_first().replace(
        "image = \"ghcr.io/acme/notes-api\"",
        "image = \"ghcr.io/acme/notes-api\"\ndepends_on = [\"db\"]",
    )
}

/// `toml` with db's config changed.
fn db_changed(toml: &str) -> String {
    toml.replace(
        "pg = \"/var/lib/postgresql\"",
        "pg = \"/var/lib/postgresql\"\n\n[services.db.env]\nC = \"1\"",
    )
}

/// A start-first service depending on a replaced stop-first one is held back: `Replacing`
/// starts and probes it once the new db has passed, never against the old one, while its own
/// old generation keeps serving until the swap.
#[tokio::test]
async fn a_start_first_service_is_held_back_behind_a_replaced_dependency() {
    let h = harness().await;
    bootstrap_with(&h, &api_on_db()).await;
    h.runtime.take_starts();
    let changed = db_changed(&api_on_db()).replace(
        "depends_on = [\"db\"]",
        "depends_on = [\"db\"]\nenv = { B = \"1\" }",
    );
    h.prober.hold(8080);
    let applied = h.apply(&changed).await;
    let id = applied.deployment.as_ref().unwrap().deployment;
    h.reaches(id, S::Replacing).await;
    h.prober.probed(8080).await;
    // The new db runs and passed; the new api is probed against it while api g2 serves.
    assert!(h.container("hoster-notes-db-g3").state.is_running());
    assert!(!h.container("hoster-notes-db-g1").state.is_running());
    assert_eq!(api(&h.routes).generation, 2);
    h.prober.release(8080);
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
    assert_eq!(
        h.runtime.take_starts(),
        ["hoster-notes-db-g3", "hoster-notes-api-g3"]
    );
    let notes = h.notes(id).await;
    let replacing = notes
        .iter()
        .position(|n| *n == (S::Replacing, "replacing db (stop-first)".to_owned()))
        .unwrap_or_else(|| panic!("{notes:#?}"));
    let held = notes
        .iter()
        .position(|n| *n == (S::Replacing, "starting api (held back for db)".to_owned()))
        .unwrap_or_else(|| panic!("{notes:#?}"));
    assert!(replacing < held, "{notes:#?}");
    assert!(
        notes
            .iter()
            .any(|(s, n)| *s == S::Replacing && n.starts_with("checking api with")),
        "{notes:#?}"
    );
    assert!(
        !notes.iter().any(|(s, _)| *s == S::HealthChecking),
        "nothing to probe before Replacing: {notes:#?}"
    );
    assert_eq!(
        (api(&h.routes).generation, db(&h.routes).generation),
        (3, 3)
    );
    h.assert_one_generation_each(&h.routes).await;
}

/// An unchanged service that exits while a dependency is replaced is started again once the
/// replacement passes, without a restart record, and probed before the deployment goes on
/// (ADR-0056 Decision 2.4).
#[tokio::test]
async fn an_unchanged_dependent_that_exited_during_the_gap_is_started_again() {
    let h = harness().await;
    bootstrap_with(&h, &api_on_db()).await;
    h.prober.hold(5432);
    let applied = h.apply(&db_changed(&api_on_db())).await;
    let id = applied.deployment.as_ref().unwrap().deployment;
    h.reaches(id, S::Replacing).await;
    h.prober.probed(5432).await;
    // api loses its database and exits.
    let api_g2 = h.container("hoster-notes-api-g2").id;
    h.runtime
        .set_state(&api_g2, ContainerState::Exited(Some(1)));
    h.runtime.take_starts();
    h.prober.release(5432);
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
    assert_eq!(h.runtime.take_starts(), ["hoster-notes-api-g2"]);
    assert!(h.container("hoster-notes-api-g2").state.is_running());
    let notes = h.notes(id).await;
    assert!(
        notes.contains(&(
            S::Replacing,
            "starting api (unchanged) again: it exited while db was replaced".to_owned()
        )),
        "{notes:#?}"
    );
    assert!(
        notes
            .iter()
            .any(|(s, n)| *s == S::Replacing && n.starts_with("checking api with")),
        "{notes:#?}"
    );
    // Not a reconciler restart: no record, no event.
    let status = h.orchestrator.status().await.unwrap();
    let api_status = status[0].services.iter().find(|s| s.name == "api").unwrap();
    assert_eq!(api_status.restarts, RestartStats::default());
    let restarts = h
        .store
        .event_page(&crate::store::EventQuery {
            kinds: [crate::store::EventKind::ContainerRestarted].into(),
            limit: crate::store::EventQuery::MAX,
            ..crate::store::EventQuery::default()
        })
        .await
        .unwrap()
        .events;
    assert!(restarts.is_empty(), "{restarts:?}");
    assert_eq!(db(&h.routes).generation, 3);
    h.assert_one_generation_each(&h.routes).await;
}

/// `Aborting` starts the stopped stop-first containers in the dependency order of the last
/// committed revision, not the failed target's, ignoring its edge to a service without a
/// pointer (ADR-0056 Decision 2.6).
#[tokio::test]
async fn an_abort_restarts_the_stopped_containers_in_committed_dependency_order() {
    const STACK: &str = r#"
version = 1
name = "notes"

[services.a]
image = "docker.io/library/busybox:1"
update = "stop-first"
depends_on = ["b", "c"]

[services.a.health]
exec = ["check"]
interval = "10ms"
start_period = "100ms"

[services.b]
image = "docker.io/library/busybox:1"
update = "stop-first"

[services.b.health]
exec = ["check"]
interval = "10ms"
start_period = "100ms"

# CI-built and never deployed: an edge to it orders nothing and holds nothing.
[services.c]
image = "ghcr.io/acme/c"
"#;
    let h = harness().await;
    let first = h
        .finish(h.apply(STACK).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(first.state, S::Live, "{first:?}");
    assert_eq!(
        h.runtime.take_starts(),
        ["hoster-notes-b-g1", "hoster-notes-a-g1"]
    );
    // The target turns the edge around and changes both, so `Replacing` goes a, then b.
    let target = STACK
        .replace("depends_on = [\"b\", \"c\"]\n", "env = { A = \"1\" }\n")
        .replace(
            "[services.b]\nimage = \"docker.io/library/busybox:1\"\n",
            "[services.b]\nimage = \"docker.io/library/busybox:1\"\ndepends_on = [\"a\"]\nenv = { B = \"1\" }\n",
        );
    h.runtime
        .script_exec("-b-g2", [Ok(ExecOutcome::finished(1, "no\n".into()))]);
    let failed = h
        .finish(h.apply(&target).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .starts_with("b: health check"),
        "{failed:?}"
    );
    assert_eq!(
        h.runtime.take_starts(),
        [
            "hoster-notes-a-g2",
            "hoster-notes-b-g2",
            "hoster-notes-b-g1",
            "hoster-notes-a-g1"
        ]
    );
    assert_eq!(
        h.container_names(),
        ["hoster-notes-a-g1", "hoster-notes-b-g1"]
    );
}

/// `two_stop_first()` with the stop-first storage depending on the start-first web.
fn storage_on_web() -> String {
    let toml = two_stop_first().replace(
        "update = \"stop-first\"\n\n[services.storage.volumes]",
        "update = \"stop-first\"\ndepends_on = [\"web\"]\n\n[services.storage.volumes]",
    );
    assert!(toml.contains("depends_on = [\"web\"]"));
    toml
}

/// An unchanged dependency must still run when a later replace-phase dependent starts: one
/// that stopped while an earlier service was replaced fails the deployment, and the abort
/// starts the old stop-first container again behind its old upstream (ADR-0056 Decision 2.4).
#[tokio::test]
async fn an_unchanged_dependency_that_stops_while_replacing_fails_the_deploy() {
    let h = harness().await;
    bootstrap_with(&h, &storage_on_web()).await;
    let before = h.container_names();
    let old_db = db(&h.routes);
    h.prober.hold(5432);
    let applied = h.apply(&both_stop_first_changed(&storage_on_web())).await;
    assert_eq!(applied.changed, ["db", "storage"]);
    let id = applied.deployment.as_ref().unwrap().deployment;
    h.reaches(id, S::Replacing).await;
    h.prober.probed(5432).await;
    // db is being replaced; web, which storage needs, goes down meanwhile.
    let web = h.container("hoster-notes-web-g2").id;
    h.runtime.set_state(&web, ContainerState::Exited(Some(1)));
    h.prober.release(5432);
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    assert_eq!(
        failed.error.as_deref(),
        Some("web (unchanged) is not running; never started: storage")
    );
    // storage was never stopped; db's old container runs again behind its old upstream.
    assert_eq!(h.container_names(), before);
    assert!(h.container("hoster-notes-db-g1").state.is_running());
    assert!(h.container("hoster-notes-storage-g1").state.is_running());
    assert!(Arc::ptr_eq(&db(&h.routes), &old_db));
}

/// One stop-first service depending on another, sorting before it by name: `Replacing` takes
/// the dependency first (ADR-0056 Decision 2.2).
#[tokio::test]
async fn replacing_follows_dependencies_before_names() {
    const STACK: &str = r#"
version = 1
name = "notes"

[services.api]
image = "docker.io/library/busybox:1"
update = "stop-first"
depends_on = ["db"]

[services.api.health]
exec = ["check"]
interval = "10ms"

[services.db]
image = "docker.io/library/busybox:1"
update = "stop-first"

[services.db.health]
exec = ["check"]
interval = "10ms"
"#;
    let h = harness().await;
    let first = h
        .finish(h.apply(STACK).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(first.state, S::Live, "{first:?}");
    assert_eq!(
        h.runtime.take_starts(),
        ["hoster-notes-db-g1", "hoster-notes-api-g1"]
    );
    let both = STACK
        .replace(
            "depends_on = [\"db\"]\n",
            "depends_on = [\"db\"]\nenv = { A = \"1\" }\n",
        )
        .replace(
            "[services.db]\nimage = \"docker.io/library/busybox:1\"\n",
            "[services.db]\nimage = \"docker.io/library/busybox:1\"\nenv = { B = \"1\" }\n",
        );
    let applied = h.apply(&both).await;
    assert_eq!(applied.changed, ["api", "db"]);
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
    assert_eq!(
        h.runtime.take_starts(),
        ["hoster-notes-db-g2", "hoster-notes-api-g2"]
    );
    let notes = h.notes(live.id).await;
    let at = |text: &str| {
        notes
            .iter()
            .position(|(_, n)| n == text)
            .unwrap_or_else(|| panic!("no note {text:?} in {notes:#?}"))
    };
    assert!(at("replacing db (stop-first)") < at("replacing api (stop-first)"));
}

/// A restarted unchanged dependent whose check fails aborts the deployment, and its exec
/// output has the secrets it references redacted, though the deployment did not create it
/// (ADR-0056 Decisions 2.4 and 4).
#[tokio::test]
async fn a_restarted_unchanged_dependent_that_fails_its_check_aborts_with_secrets_redacted() {
    const STACK: &str = r#"
version = 1
name = "notes"

[services.db]
image = "docker.io/library/postgres:17"
update = "stop-first"

[[services.db.routes]]
host = "db.notes.example.com"
port = 5432

[services.db.health]
tcp = { port = 5432 }
interval = "10ms"
start_period = "200ms"

[services.worker]
image = "docker.io/acme/worker:1"
depends_on = ["db"]
secrets = { DB_PASSWORD = "notes/db-password" }

[services.worker.health]
exec = ["sh", "-c", "env; exit 1"]
interval = "10ms"
start_period = "100ms"
"#;
    let h = harness().await;
    h.set_secret("notes/db-password", "hunter22").await;
    let first = h
        .finish(h.apply(STACK).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(first.state, S::Live, "{first:?}");

    h.runtime.script_exec(
        "-worker-",
        [Ok(ExecOutcome::finished(
            1,
            "DB_PASSWORD=hunter22\n".into(),
        ))],
    );
    h.prober.hold(5432);
    let changed = STACK.replace(
        "update = \"stop-first\"\n",
        "update = \"stop-first\"\nenv = { C = \"1\" }\n",
    );
    let applied = h.apply(&changed).await;
    assert_eq!(applied.changed, ["db"]);
    let id = applied.deployment.as_ref().unwrap().deployment;
    h.reaches(id, S::Replacing).await;
    h.prober.probed(5432).await;
    let worker = h.container("hoster-notes-worker-g1").id;
    h.runtime
        .set_state(&worker, ContainerState::Exited(Some(1)));
    h.runtime.take_starts();
    h.prober.release(5432);
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    let error = failed.error.as_deref().unwrap();
    assert!(
        error.starts_with("worker (unchanged): health check"),
        "{error}"
    );
    // worker was started again and checked; the abort starts the old db again.
    assert_eq!(
        h.runtime.take_starts(),
        ["hoster-notes-worker-g1", "hoster-notes-db-g1"]
    );
    assert!(h.container("hoster-notes-db-g1").state.is_running());
    assert_eq!(
        h.container_names(),
        ["hoster-notes-db-g1", "hoster-notes-worker-g1"]
    );
    let notes: Vec<String> = h.notes(id).await.into_iter().map(|(_, n)| n).collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("output \"DB_PASSWORD=<redacted>\"")),
        "{notes:#?}"
    );
    assert!(!notes.iter().any(|n| n.contains("hunter22")), "{notes:#?}");
}

/// An unchanged dependent that was not running when the deployment began is left alone: it
/// is the reconciler's (ADR-0056 Decision 2.4).
#[tokio::test]
async fn an_unchanged_dependent_already_down_is_not_started_by_the_deploy() {
    let h = harness().await;
    bootstrap_with(&h, &api_on_db()).await;
    let api_g2 = h.container("hoster-notes-api-g2").id;
    h.runtime
        .set_state(&api_g2, ContainerState::Exited(Some(1)));
    h.runtime.take_starts();
    let live = h
        .finish(
            h.apply(&db_changed(&api_on_db()))
                .await
                .deployment
                .as_ref()
                .unwrap(),
        )
        .await;
    assert_eq!(live.state, S::Live, "{live:?}");
    assert_eq!(h.runtime.take_starts(), ["hoster-notes-db-g3"]);
    assert!(!h.container("hoster-notes-api-g2").state.is_running());
}

/// A held-back service depending on an unchanged one that is started again after the gap
/// starts only once that one has passed its check (ADR-0056 Decision 2.4).
#[tokio::test]
async fn a_held_back_service_waits_for_its_restarted_unchanged_dependency() {
    let h = harness().await;
    let toml = api_on_db().replace(
        "[services.web]\nimage = \"ghcr.io/acme/notes-web\"",
        "[services.web]\nimage = \"ghcr.io/acme/notes-web\"\ndepends_on = [\"api\"]",
    );
    assert!(toml.contains("depends_on = [\"api\"]"));
    bootstrap_with(&h, &toml).await;
    let changed = db_changed(&toml).replace(
        "depends_on = [\"api\"]",
        "depends_on = [\"api\"]\nenv = { D = \"1\" }",
    );
    h.prober.hold(5432);
    let applied = h.apply(&changed).await;
    assert_eq!(applied.changed, ["db", "web"]);
    let id = applied.deployment.as_ref().unwrap().deployment;
    h.reaches(id, S::Replacing).await;
    h.prober.probed(5432).await;
    let api_g2 = h.container("hoster-notes-api-g2").id;
    h.runtime
        .set_state(&api_g2, ContainerState::Exited(Some(1)));
    h.runtime.take_starts();
    h.prober.release(5432);
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
    assert_eq!(
        h.runtime.take_starts(),
        ["hoster-notes-api-g2", "hoster-notes-web-g3"]
    );
    let notes = h.notes(id).await;
    let at = |test: &dyn Fn(&str) -> bool| {
        notes
            .iter()
            .position(|(_, n)| test(n))
            .unwrap_or_else(|| panic!("{notes:#?}"))
    };
    let restarted = at(&|n| n == "starting api (unchanged) again: it exited while db was replaced");
    let passed = at(&|n| n.contains("container port 8080") && n.contains(" passed "));
    let web = at(&|n| n == "starting web (held back for db)");
    assert!(restarted < passed && passed < web, "{notes:#?}");
    h.assert_one_generation_each(&h.routes).await;
}
