use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// A [`StatsSource`] serving whatever the test puts in it.
#[derive(Debug, Default)]
pub(crate) struct FakeStats {
    pub deployments: Mutex<Vec<DeploymentOutcome>>,
    /// Reading the deployments fails.
    pub broken: AtomicBool,
    pub services: Mutex<Vec<ServiceRestarts>>,
    pub certificates: Mutex<Vec<CertExpiry>>,
}

#[async_trait]
impl StatsSource for FakeStats {
    async fn deployments(&self) -> anyhow::Result<Vec<DeploymentOutcome>> {
        if self.broken.load(Ordering::Relaxed) {
            anyhow::bail!("database is locked");
        }
        Ok(self.deployments.lock().unwrap().clone())
    }

    async fn services(&self) -> anyhow::Result<Vec<ServiceRestarts>> {
        Ok(self.services.lock().unwrap().clone())
    }

    fn certificates(&self) -> Vec<CertExpiry> {
        self.certificates.lock().unwrap().clone()
    }
}

fn outcome(stack: &str, state: DeploymentState, count: u32, mean_s: u64) -> DeploymentOutcome {
    DeploymentOutcome {
        stack: stack.into(),
        state,
        count,
        mean: Duration::from_secs(mean_s),
        max: Duration::from_secs(mean_s * 2),
    }
}

#[test]
fn deployment_outcomes_are_grouped_per_stack() {
    let stacks = DeployStats::group(vec![
        outcome("blog", DeploymentState::Failed, 1, 9),
        outcome("blog", DeploymentState::Live, 3, 10),
        outcome("blog", DeploymentState::LiveWithWarnings, 1, 30),
        outcome("blog", DeploymentState::Noop, 4, 0),
        outcome("shop", DeploymentState::Rejected, 2, 0),
    ]);
    let [blog, shop] = stacks.as_slice() else {
        panic!("{stacks:?}");
    };
    assert_eq!(blog.stack, "blog");
    assert_eq!((blog.succeeded(), blog.failed(), blog.skipped()), (4, 1, 4));
    // (3 × 10 s + 1 × 30 s) / 4
    assert_eq!(blog.mean_live_duration(), Some(Duration::from_secs(15)));
    assert_eq!(blog.max_live_duration(), Some(Duration::from_secs(60)));
    assert_eq!((shop.succeeded(), shop.failed()), (0, 2));
    assert_eq!(shop.mean_live_duration(), None);
}

#[tokio::test]
async fn a_snapshot_combines_traffic_and_sources() {
    let source = Arc::new(FakeStats::default());
    source
        .deployments
        .lock()
        .unwrap()
        .push(outcome("blog", DeploymentState::Live, 2, 4));
    source.services.lock().unwrap().extend([
        ServiceRestarts {
            stack: "blog".into(),
            service: "web".into(),
            generation: 3,
            restarts: 6,
            crash_looping: true,
        },
        ServiceRestarts {
            stack: "blog".into(),
            service: "worker".into(),
            generation: 3,
            restarts: 0,
            crash_looping: false,
        },
    ]);
    let now = SystemTime::now();
    let day = Duration::from_secs(86_400);
    source.certificates.lock().unwrap().extend([
        CertExpiry {
            host: "late.example.com".into(),
            not_after: now + day * 80,
        },
        CertExpiry {
            host: "soon.example.com".into(),
            not_after: now + day * 3,
        },
    ]);
    let traffic = Traffic::default();
    traffic.unrouted().finish(RequestOutcome {
        status: Some(404),
        latency: Duration::from_millis(1),
        upstream_error: false,
    });
    let stats = Stats::new(traffic, source);

    let snapshot = stats.snapshot().await;
    assert!(snapshot.problems.is_empty(), "{:?}", snapshot.problems);
    assert_eq!(snapshot.traffic.unrouted.statuses.total(), 1);
    assert_eq!(snapshot.deployments[0].succeeded(), 2);
    let looping: Vec<_> = snapshot
        .services
        .iter()
        .filter(|s| s.crash_looping)
        .map(|s| s.service.as_str())
        .collect();
    assert_eq!(looping, ["web"]);
    assert_eq!(snapshot.certificates[0].host, "soon.example.com");
    let left = snapshot.certificates[0].remaining(now);
    assert!(left <= day * 3 && left > day * 2, "{left:?}");
}

#[tokio::test]
async fn a_failing_source_leaves_the_rest_of_the_snapshot() {
    let source = FakeStats::default();
    source.broken.store(true, Ordering::Relaxed);
    let stats = Stats::new(Traffic::default(), Arc::new(source));
    let snapshot = stats.snapshot().await;
    assert!(snapshot.deployments.is_empty());
    assert_eq!(snapshot.problems.len(), 1);
    assert!(
        snapshot.problems[0].contains("database is locked"),
        "{:?}",
        snapshot.problems
    );
}

#[test]
fn expired_certificates_have_no_time_left() {
    let cert = CertExpiry {
        host: "old.example.com".into(),
        not_after: SystemTime::UNIX_EPOCH,
    };
    assert_eq!(cert.remaining(SystemTime::now()), Duration::ZERO);
}

#[test]
fn durations_read_well() {
    let cases = [
        (Duration::from_micros(800), "0.8 ms"),
        (Duration::from_millis(12), "12 ms"),
        (Duration::from_millis(1400), "1.4 s"),
        (Duration::from_secs(185), "3 min 5 s"),
        (Duration::from_secs(2 * 3600 + 60), "2 h 1 min"),
        (Duration::from_secs(2 * 86_400 + 4 * 3600), "2 d 4 h"),
        (Duration::from_secs(300), "5 min"),
        (Duration::from_secs(3 * 3600 + 59), "3 h"),
        (Duration::from_secs(30 * 86_400), "30 d"),
    ];
    for (duration, text) in cases {
        assert_eq!(Elapsed(duration).to_string(), text);
    }
}
