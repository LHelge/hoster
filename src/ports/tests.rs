use std::sync::Arc;

use tempfile::TempDir;

use super::*;

/// The smallest allowed range: 20000-20015.
fn small_range() -> PortRange {
    PortRange::try_from(20000..=20015).unwrap()
}

async fn open(dir: &TempDir) -> Store {
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    for stack in ["blog", "shop"] {
        store.add_revision(stack, "x").await.unwrap();
    }
    store
}

async fn allocator(range: PortRange) -> (TempDir, PortAllocator) {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    (dir, PortAllocator::new(store, range))
}

#[test]
fn round_robin_starts_after_the_cursor_and_wraps() {
    let range = small_range();
    let walk = |cursor| RoundRobin { range, cursor }.into_iter().collect::<Vec<_>>();
    let all: Vec<u16> = (20000..=20015).collect();

    assert_eq!(walk(None), all);
    // The cursor at the last port wraps to the first.
    assert_eq!(walk(Some(20015)), all);
    // A cursor outside the range (reconfigured) starts at the first port.
    assert_eq!(walk(Some(19000)), all);
    assert_eq!(walk(Some(30000)), all);

    let from_middle = walk(Some(20007));
    assert_eq!(from_middle.len(), range.len());
    assert_eq!(from_middle[0], 20008);
    assert_eq!(from_middle[7], 20015);
    assert_eq!(from_middle[8], 20000);
    assert_eq!(from_middle[15], 20007);
}

#[test]
fn bad_ports_expire_after_the_quarantine() {
    let start = Instant::now();
    let mut bad = BadPorts::default();
    bad.mark(20003, start);
    assert_eq!(bad.current(start), BTreeSet::from([20003]));
    let almost = start + PortAllocator::QUARANTINE - Duration::from_secs(1);
    assert_eq!(bad.current(almost), BTreeSet::from([20003]));
    assert!(bad.current(start + PortAllocator::QUARANTINE).is_empty());
    // An expired mark is dropped for good.
    assert!(bad.current(start).is_empty());
}

#[tokio::test]
async fn default_range_starts_at_20000() {
    let (_dir, ports) = allocator(PortRange::default()).await;
    assert_eq!(ports.range().first(), 20000);
    assert_eq!(ports.range().last(), 29999);
    assert_eq!(ports.allocate("blog", "api", 8080, 1).await.unwrap(), 20000);
    assert_eq!(ports.allocate("blog", "api", 8080, 2).await.unwrap(), 20001);
}

#[tokio::test]
async fn allocations_are_recorded_and_listed() {
    let (_dir, ports) = allocator(small_range()).await;
    let a = ports.allocate("blog", "api", 8080, 1).await.unwrap();
    let b = ports.allocate("shop", "web", 3000, 4).await.unwrap();
    assert_eq!(
        ports.list().await.unwrap(),
        [
            PortAllocation {
                port: a,
                stack: "blog".into(),
                service: "api".into(),
                container_port: 8080,
                generation: 1,
            },
            PortAllocation {
                port: b,
                stack: "shop".into(),
                service: "web".into(),
                container_port: 3000,
                generation: 4,
            },
        ]
    );
    assert!(ports.release(a).await.unwrap());
    assert!(!ports.release(a).await.unwrap());
    assert_eq!(ports.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn released_ports_are_not_reused_right_away() {
    let (_dir, ports) = allocator(small_range()).await;
    assert_eq!(ports.allocate("blog", "api", 8080, 1).await.unwrap(), 20000);
    assert_eq!(ports.allocate("blog", "api", 8080, 2).await.unwrap(), 20001);
    // Generation 1 is drained and its port released; the next deploy still moves on.
    assert!(ports.release(20000).await.unwrap());
    assert_eq!(ports.allocate("blog", "api", 8080, 3).await.unwrap(), 20002);

    // Fill the rest of the range; the walk then wraps to the released 20000.
    for generation in 4..=16 {
        ports
            .allocate("blog", "api", 8080, generation)
            .await
            .unwrap();
    }
    assert_eq!(
        ports.allocate("blog", "api", 8080, 17).await.unwrap(),
        20000
    );
}

#[tokio::test]
async fn exhaustion_is_a_typed_error() {
    let range = small_range();
    let (_dir, ports) = allocator(range).await;
    for generation in 1..=16 {
        ports
            .allocate("blog", "api", 8080, generation)
            .await
            .unwrap();
    }
    let err = ports.allocate("blog", "api", 8080, 17).await.unwrap_err();
    assert!(matches!(err, PortError::Exhausted { range: r } if r == range));
    assert_eq!(ports.list().await.unwrap().len(), 16);

    // Releasing one makes exactly that one available again.
    ports.release(20009).await.unwrap();
    assert_eq!(
        ports.allocate("blog", "api", 8080, 17).await.unwrap(),
        20009
    );
    assert!(matches!(
        ports.allocate("blog", "api", 8080, 18).await,
        Err(PortError::Exhausted { .. })
    ));
}

#[tokio::test]
async fn unknown_stacks_are_refused() {
    let (_dir, ports) = allocator(small_range()).await;
    let err = ports.allocate("ghost", "api", 8080, 1).await.unwrap_err();
    assert!(matches!(err, PortError::Store(StoreError::UnknownStack(s)) if s == "ghost"));
    assert!(ports.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn bad_ports_are_released_and_skipped() {
    let (_dir, ports) = allocator(small_range()).await;
    let port = ports.allocate("blog", "api", 8080, 1).await.unwrap();
    assert_eq!(port, 20000);
    // podman reported "address in use" for 20000.
    ports.mark_bad(port).await.unwrap();
    assert!(ports.list().await.unwrap().is_empty());
    assert_eq!(ports.allocate("blog", "api", 8080, 1).await.unwrap(), 20001);

    // When the walk wraps, the quarantined port is still skipped.
    for generation in 2..=15 {
        ports
            .allocate("blog", "api", 8080, generation)
            .await
            .unwrap();
    }
    assert!(matches!(
        ports.allocate("blog", "api", 8080, 16).await,
        Err(PortError::Exhausted { .. })
    ));
}

#[tokio::test]
async fn allocations_and_cursor_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let ports = PortAllocator::new(store.clone(), small_range());
    for generation in 1..=3 {
        ports
            .allocate("blog", "api", 8080, generation)
            .await
            .unwrap();
    }
    ports.release(20000).await.unwrap();
    let before = ports.list().await.unwrap();
    drop(ports);
    store.close().await;

    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let ports = PortAllocator::new(store, small_range());
    assert_eq!(ports.list().await.unwrap(), before);
    // The cursor was persisted too: the next port follows the last one handed out.
    assert_eq!(ports.allocate("blog", "api", 8080, 4).await.unwrap(), 20003);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_allocations_never_share_a_port() {
    let (_dir, ports) = allocator(PortRange::default()).await;
    let ports = Arc::new(ports);
    let tasks: Vec<_> = (0..40u32)
        .map(|i| {
            let ports = Arc::clone(&ports);
            let stack = if i % 2 == 0 { "blog" } else { "shop" };
            tokio::spawn(async move { ports.allocate(stack, "api", 8080, i + 1).await.unwrap() })
        })
        .collect();
    let mut got = BTreeSet::new();
    for task in tasks {
        assert!(got.insert(task.await.unwrap()), "port handed out twice");
    }
    assert_eq!(got, (20000..20040).collect());
    assert_eq!(ports.list().await.unwrap().len(), 40);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_allocators_on_one_store_never_share_a_port() {
    // Two handles (as two stacks' deploys would use) contend for the same small range.
    let dir = TempDir::new().unwrap();
    let store = open(&dir).await;
    let a = Arc::new(PortAllocator::new(store.clone(), small_range()));
    let b = Arc::new(PortAllocator::new(store, small_range()));
    let tasks: Vec<_> = (0..20u32)
        .map(|i| {
            let (ports, stack) = if i % 2 == 0 {
                (&a, "blog")
            } else {
                (&b, "shop")
            };
            let ports = Arc::clone(ports);
            tokio::spawn(async move { ports.allocate(stack, "api", 8080, i + 1).await })
        })
        .collect();
    let mut got = BTreeSet::new();
    let mut exhausted = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(port) => assert!(got.insert(port), "port {port} handed out twice"),
            Err(PortError::Exhausted { .. }) => exhausted += 1,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert_eq!(got.len(), 16);
    assert_eq!(exhausted, 4);
}
