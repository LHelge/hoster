//! The receiver's per-service limits (ADR-0069 §7): a token bucket per service for log records
//! and one for spans, a cap on its concurrent requests, and what the buckets dropped.
//!
//! What a bucket has no token for is dropped. Each run of drops of one signal by one service is
//! a [`Gap`], which ends, and becomes one marker record that says how many were dropped (the
//! pattern of ADR-0067 §7's line-rate cap), when the next export of that signal is delivered
//! whole, when it is [`OtlpLimits::GAP_MAX`] old, or when the service's state is let go.
//!
//! State is kept per service in one map and let go once the service has been idle for
//! [`OtlpLimits::IDLE`]: a bucket idle that long is full again, so forgetting it changes nothing.
//! A service that lost anything keeps its drop counts for the System page while it exists. The
//! map is swept at most every [`OtlpLimits::SWEEP_INTERVAL`], on an export from any service.
//!
//! Every method takes the time it acts at, so tests drive the buckets without sleeping.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use crate::secrets::{KnownServices, ServiceIdentity};
use crate::store::{Level, LogRecord, Stream, Timestamp};

/// The per-service limits of the OTLP receiver and what they dropped since hoster started.
/// Cheap to clone; every clone shares the state.
#[derive(Debug, Clone, Default)]
pub struct OtlpLimits(Arc<Mutex<Services>>);

/// What a service's rate caps dropped since hoster started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capped {
    pub records: u64,
    pub spans: u64,
}

impl Capped {
    /// Both together.
    pub fn total(&self) -> u64 {
        self.records + self.spans
    }
}

/// The two signals the receiver takes, each with its own bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    Records,
    Spans,
}

impl Signal {
    /// Tokens added per second.
    pub(super) fn per_second(self) -> u64 {
        match self {
            Signal::Records => OtlpLimits::RECORDS_PER_SECOND,
            Signal::Spans => OtlpLimits::SPANS_PER_SECOND,
        }
    }

    /// The most tokens a bucket holds.
    fn burst(self) -> u64 {
        self.per_second() * OtlpLimits::BURST_SECONDS
    }
}

impl Signal {
    fn plural(self) -> &'static str {
        match self {
            Signal::Records => "log records",
            Signal::Spans => "spans",
        }
    }
}

/// A token bucket of one signal. Tokens are counted in millionths, so refilling needs no
/// floating point: a microsecond adds `per_second` millionths.
#[derive(Debug, Clone, Copy)]
struct TokenBucket {
    micro_tokens: u64,
    updated: Instant,
}

impl TokenBucket {
    const SCALE: u64 = 1_000_000;

    /// A full bucket for `signal`.
    fn full(signal: Signal, now: Instant) -> Self {
        Self {
            micro_tokens: signal.burst() * Self::SCALE,
            updated: now,
        }
    }

    /// Takes up to `wanted` tokens at `now`; how many it had.
    fn take(&mut self, signal: Signal, wanted: u64, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.updated);
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.micro_tokens = self
            .micro_tokens
            .saturating_add(micros.saturating_mul(signal.per_second()))
            .min(signal.burst() * Self::SCALE);
        self.updated = self.updated.max(now);
        let taken = wanted.min(self.micro_tokens / Self::SCALE);
        self.micro_tokens -= taken * Self::SCALE;
        taken
    }
}

/// A run of one signal's items dropped by one service's bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Gap {
    dropped: u64,
    /// When the first was dropped, by the monotonic clock.
    opened: Instant,
    /// When the last was received: the marker is stamped with it, so it sorts inside the gap.
    last: SystemTime,
}

/// One gap that ended: a marker record to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Marker {
    identity: ServiceIdentity,
    signal: Signal,
    gap: Gap,
}

impl Marker {
    /// `1200 spans dropped: more than 1000 spans a second over OTLP`.
    fn message(&self) -> String {
        let what = match (self.signal, self.gap.dropped) {
            (Signal::Records, 1) => "log record",
            (Signal::Spans, 1) => "span",
            (signal, _) => signal.plural(),
        };
        format!(
            "{} {what} dropped: more than {} {} a second over OTLP",
            self.gap.dropped,
            self.signal.per_second(),
            self.signal.plural(),
        )
    }
}

impl From<Marker> for LogRecord {
    fn from(marker: Marker) -> Self {
        let message = marker.message();
        let time = Timestamp::from(marker.gap.last);
        Self {
            time,
            stack: Some(marker.identity.stack.to_string()),
            service: marker.identity.service.to_string(),
            generation: None,
            stream: Stream::Otlp,
            level: Some(Level::Warn),
            message,
            fields: None,
            cut: false,
            marker: true,
            trace_id: None,
            span_id: None,
            received: Some(time),
        }
    }
}

/// One service's state.
#[derive(Debug)]
struct ServiceState {
    records: TokenBucket,
    spans: TokenBucket,
    record_gap: Option<Gap>,
    span_gap: Option<Gap>,
    /// Requests being served now.
    in_flight: usize,
    /// When the last request came.
    seen: Instant,
    capped: Capped,
}

impl ServiceState {
    fn new(now: Instant) -> Self {
        Self {
            records: TokenBucket::full(Signal::Records, now),
            spans: TokenBucket::full(Signal::Spans, now),
            record_gap: None,
            span_gap: None,
            in_flight: 0,
            seen: now,
            capped: Capped::default(),
        }
    }

    fn signal(&mut self, signal: Signal) -> (&mut TokenBucket, &mut Option<Gap>, &mut u64) {
        match signal {
            Signal::Records => (
                &mut self.records,
                &mut self.record_gap,
                &mut self.capped.records,
            ),
            Signal::Spans => (&mut self.spans, &mut self.span_gap, &mut self.capped.spans),
        }
    }

    /// The gaps it has open, ended.
    fn end_gaps(&mut self, identity: &ServiceIdentity) -> impl Iterator<Item = Marker> {
        let records = self.record_gap.take().map(|gap| Marker {
            identity: identity.clone(),
            signal: Signal::Records,
            gap,
        });
        let spans = self.span_gap.take().map(|gap| Marker {
            identity: identity.clone(),
            signal: Signal::Spans,
            gap,
        });
        records.into_iter().chain(spans)
    }
}

#[derive(Debug, Default)]
struct Services {
    states: HashMap<ServiceIdentity, ServiceState>,
    /// When the map was last swept; `None` before the first sweep.
    swept: Option<Instant>,
}

/// What [`OtlpLimits::admit`] decided about one export.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Admission {
    /// The items kept, from the first: the rest are dropped.
    pub kept: usize,
    /// Gaps that ended, to mark before the kept items.
    pub markers: Vec<Marker>,
}

/// A request counted against its service's concurrency cap until it is dropped.
#[derive(Debug)]
pub(super) struct InFlight {
    limits: OtlpLimits,
    identity: ServiceIdentity,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some(state) = self.limits.lock().states.get_mut(&self.identity) {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }
}

/// The service has [`OtlpLimits::MAX_IN_FLIGHT`] requests being served already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Busy;

impl OtlpLimits {
    /// Log records a service may send per second, sustained. The same as a container's
    /// line-rate cap (ADR-0067 §7), so a service logging over OTLP has the budget it would have
    /// printing: a hundred times what a busy app logs while serving, and well within what one
    /// recorder commits on a small VPS.
    pub const RECORDS_PER_SECOND: u64 = 1_000;
    /// Spans a service may send per second, sustained. A traced request is typically 5 to 20
    /// spans, so this holds a service at 50 to 200 traced requests a second, more than one
    /// small VPS serves for one app; a service's two caps together stay well under the
    /// recorder's channel of 16,384.
    pub const SPANS_PER_SECOND: u64 = 1_000;
    /// A bucket holds this many seconds of its rate, so an SDK's batch exporter, which sends
    /// every 1 s (logs) or 5 s (traces) in batches of up to 512, is never cut while its
    /// average stays under the rate.
    pub const BURST_SECONDS: u64 = 5;
    /// Requests of one service served at once; the next is `429`. SDKs export one batch per
    /// signal at a time, so this leaves room for a few processes in one container, while
    /// bounding what one service holds in memory to four bodies (each at most
    /// [`OtlpReceiver::MAX_BODY`] as sent and [`OtlpReceiver::MAX_DECODED`] inflated).
    ///
    /// [`OtlpReceiver::MAX_BODY`]: super::OtlpReceiver::MAX_BODY
    /// [`OtlpReceiver::MAX_DECODED`]: super::OtlpReceiver::MAX_DECODED
    pub const MAX_IN_FLIGHT: usize = 4;
    /// The `Retry-After` of a `429`: requests take milliseconds, so a slot is free again long
    /// before this, and an SDK that honours it backs off at least this long.
    pub const RETRY_AFTER: Duration = Duration::from_secs(1);
    /// A gap ends at this age even while drops go on, so sustained overload writes one marker
    /// per signal and service a minute, not one per export.
    pub const GAP_MAX: Duration = Duration::from_secs(60);
    /// A service idle this long has its buckets let go (they would be full again anyway).
    pub const IDLE: Duration = Duration::from_secs(10 * 60);
    /// The most often the map is swept for ended gaps and idle services.
    pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

    /// Drops counted since hoster started, per service that lost any; a service's counts go
    /// once it has been idle for [`Self::IDLE`] and no longer exists.
    pub fn capped(&self) -> Vec<(ServiceIdentity, Capped)> {
        let mut capped: Vec<_> = self
            .lock()
            .states
            .iter()
            .filter(|(_, state)| state.capped.total() > 0)
            .map(|(identity, state)| (identity.clone(), state.capped))
            .collect();
        capped.sort_by(|a, b| a.0.cmp(&b.0));
        capped
    }

    /// Counts a request of `identity` arriving at `now`; [`Busy`] when it has
    /// [`Self::MAX_IN_FLIGHT`] already.
    pub(super) fn enter(&self, identity: &ServiceIdentity, now: Instant) -> Result<InFlight, Busy> {
        let mut services = self.lock();
        let state = services
            .states
            .entry(identity.clone())
            .or_insert_with(|| ServiceState::new(now));
        state.seen = state.seen.max(now);
        if state.in_flight >= Self::MAX_IN_FLIGHT {
            return Err(Busy);
        }
        state.in_flight += 1;
        Ok(InFlight {
            limits: self.clone(),
            identity: identity.clone(),
        })
    }

    /// Takes tokens for `count` items of `signal` that `identity` sent, received at `now`
    /// (`at` by the wall clock). What has no token is dropped and counted in the open gap; an
    /// export delivered whole ends the gap, as does the gap's age.
    pub(super) fn admit(
        &self,
        identity: &ServiceIdentity,
        signal: Signal,
        count: usize,
        now: Instant,
        at: SystemTime,
    ) -> Admission {
        let mut services = self.lock();
        let state = services
            .states
            .entry(identity.clone())
            .or_insert_with(|| ServiceState::new(now));
        state.seen = state.seen.max(now);
        let (bucket, gap, capped) = state.signal(signal);
        let wanted = u64::try_from(count).unwrap_or(u64::MAX);
        let kept = bucket.take(signal, wanted, now);
        let dropped = wanted - kept;
        let mut markers = Vec::new();
        if dropped > 0 {
            *capped += dropped;
            let open = gap.get_or_insert(Gap {
                dropped: 0,
                opened: now,
                last: at,
            });
            open.dropped += dropped;
            open.last = open.last.max(at);
        }
        let ends = gap.is_some_and(|gap| {
            dropped == 0 || now.saturating_duration_since(gap.opened) >= Self::GAP_MAX
        });
        if ends && let Some(gap) = gap.take() {
            markers.push(Marker {
                identity: identity.clone(),
                signal,
                gap,
            });
        }
        Admission {
            kept: usize::try_from(kept).unwrap_or(count),
            markers,
        }
    }

    /// At most every [`Self::SWEEP_INTERVAL`]: ends gaps [`Self::GAP_MAX`] old, and lets go of
    /// services idle for [`Self::IDLE`] with no request in flight (ending their gaps), keeping
    /// the drop counts of those `known` still has. The markers of the gaps it ended.
    pub(super) fn sweep(&self, now: Instant, known: &dyn KnownServices) -> Vec<Marker> {
        let mut guard = self.lock();
        let services = &mut *guard;
        if services
            .swept
            .is_some_and(|swept| now.saturating_duration_since(swept) < Self::SWEEP_INTERVAL)
        {
            return Vec::new();
        }
        services.swept = Some(now);
        let mut markers = Vec::new();
        services.states.retain(|identity, state| {
            let idle =
                state.in_flight == 0 && now.saturating_duration_since(state.seen) >= Self::IDLE;
            if idle {
                markers.extend(state.end_gaps(identity));
                return state.capped.total() > 0
                    && known.contains(&identity.stack, &identity.service);
            }
            for signal in [Signal::Records, Signal::Spans] {
                let (_, gap, _) = state.signal(signal);
                if let Some(old) =
                    gap.take_if(|gap| now.saturating_duration_since(gap.opened) >= Self::GAP_MAX)
                {
                    markers.push(Marker {
                        identity: identity.clone(),
                        signal,
                        gap: old,
                    });
                }
            }
            true
        });
        markers
    }

    fn lock(&self) -> MutexGuard<'_, Services> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::stack::Name;

    fn identity(stack: &str, service: &str) -> ServiceIdentity {
        ServiceIdentity {
            stack: stack.parse().unwrap(),
            service: service.parse().unwrap(),
        }
    }

    struct Known(BTreeSet<(Name, Name)>);

    impl KnownServices for Known {
        fn contains(&self, stack: &Name, service: &Name) -> bool {
            self.0.contains(&(stack.clone(), service.clone()))
        }
    }

    fn known(services: &[(&str, &str)]) -> Known {
        Known(
            services
                .iter()
                .map(|(stack, service)| (stack.parse().unwrap(), service.parse().unwrap()))
                .collect(),
        )
    }

    fn wall(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_794_571_200 + secs)
    }

    const BURST: usize = 5_000;

    #[test]
    fn a_bucket_holds_its_burst_and_refills_at_its_rate() {
        let start = Instant::now();
        let mut bucket = TokenBucket::full(Signal::Spans, start);
        assert_eq!(Signal::Spans.burst(), 5_000);
        assert_eq!(bucket.take(Signal::Spans, 6_000, start), 5_000);
        assert_eq!(bucket.take(Signal::Spans, 1, start), 0);
        // A millisecond is one token at 1000 a second; half of one is none yet.
        assert_eq!(
            bucket.take(Signal::Spans, 10, start + Duration::from_micros(500)),
            0
        );
        assert_eq!(
            bucket.take(Signal::Spans, 10, start + Duration::from_millis(1)),
            1
        );
        assert_eq!(
            bucket.take(Signal::Spans, 600, start + Duration::from_millis(501)),
            500
        );
        // Never more than the burst, however long it was idle.
        let later = start + Duration::from_secs(3_600);
        assert_eq!(bucket.take(Signal::Spans, 10_000, later), 5_000);
        // A time before the last one adds nothing.
        assert_eq!(bucket.take(Signal::Spans, 1, start), 0);
    }

    #[test]
    fn the_excess_is_dropped_counted_and_marked_once_when_an_export_goes_through_whole() {
        let limits = OtlpLimits::default();
        let web = identity("shop", "web");
        let start = Instant::now();
        let first = limits.admit(&web, Signal::Records, BURST + 3, start, wall(0));
        assert_eq!(first.kept, BURST);
        assert!(first.markers.is_empty());
        // Still over: the gap grows, no marker yet.
        let second = limits.admit(&web, Signal::Records, 2, start, wall(1));
        assert_eq!(second.kept, 0);
        assert!(second.markers.is_empty());
        // The spans' bucket is another one.
        let spans = limits.admit(&web, Signal::Spans, 10, start, wall(1));
        assert_eq!(spans.kept, 10);
        assert!(spans.markers.is_empty());
        assert_eq!(
            limits.capped(),
            vec![(
                web.clone(),
                Capped {
                    records: 5,
                    spans: 0
                }
            )]
        );
        // A second later, an export fits: the gap ends with it.
        let back = limits.admit(
            &web,
            Signal::Records,
            10,
            start + Duration::from_secs(1),
            wall(2),
        );
        assert_eq!(back.kept, 10);
        let [marker] = <[Marker; 1]>::try_from(back.markers).unwrap();
        assert_eq!(marker.gap.dropped, 5);
        assert_eq!(marker.gap.last, wall(1));
        let record = LogRecord::from(marker);
        assert!(record.marker);
        assert_eq!(record.stream, Stream::Otlp);
        assert_eq!(record.level, Some(Level::Warn));
        assert_eq!(record.stack.as_deref(), Some("shop"));
        assert_eq!(record.service, "web");
        assert_eq!(record.time, Timestamp::from(wall(1)));
        assert_eq!(
            record.message,
            "5 log records dropped: more than 1000 log records a second over OTLP"
        );
        // The next one has nothing to mark.
        let after = limits.admit(
            &web,
            Signal::Records,
            1,
            start + Duration::from_secs(1),
            wall(2),
        );
        assert!(after.markers.is_empty());
    }

    #[test]
    fn sustained_overload_is_marked_once_a_minute() {
        let limits = OtlpLimits::default();
        let web = identity("shop", "web");
        let start = Instant::now();
        let mut markers = Vec::new();
        // Twice the rate for two minutes, an export a second.
        for second in 0..120 {
            let now = start + Duration::from_secs(second);
            let admission = limits.admit(&web, Signal::Spans, 2_000 + BURST, now, wall(second));
            assert!(admission.kept > 0);
            markers.extend(admission.markers);
        }
        assert_eq!(markers.len(), 1, "{markers:?}");
        assert_eq!(markers[0].gap.last, wall(60));
        assert_eq!(markers[0].signal, Signal::Spans);
        assert!(
            LogRecord::from(markers[0].clone())
                .message
                .ends_with("spans dropped: more than 1000 spans a second over OTLP")
        );
    }

    #[test]
    fn a_single_dropped_item_is_named_in_the_singular() {
        let limits = OtlpLimits::default();
        let web = identity("shop", "web");
        let start = Instant::now();
        limits.admit(&web, Signal::Spans, BURST + 1, start, wall(0));
        let back = limits.admit(
            &web,
            Signal::Spans,
            1,
            start + Duration::from_secs(1),
            wall(1),
        );
        assert_eq!(
            LogRecord::from(back.markers[0].clone()).message,
            "1 span dropped: more than 1000 spans a second over OTLP"
        );
    }

    #[test]
    fn concurrent_requests_are_capped_per_service() {
        let limits = OtlpLimits::default();
        let web = identity("shop", "web");
        let worker = identity("shop", "worker");
        let now = Instant::now();
        let held: Vec<InFlight> = (0..OtlpLimits::MAX_IN_FLIGHT)
            .map(|_| limits.enter(&web, now).unwrap())
            .collect();
        assert_eq!(limits.enter(&web, now).unwrap_err(), Busy);
        // Another service has its own.
        let _other = limits.enter(&worker, now).unwrap();
        drop(held);
        let _again = limits.enter(&web, now).unwrap();
    }

    #[test]
    fn a_sweep_ends_old_gaps_and_lets_idle_services_go() {
        let limits = OtlpLimits::default();
        let web = identity("shop", "web");
        let worker = identity("shop", "worker");
        let gone = identity("old", "api");
        let busy = identity("shop", "busy");
        let known = known(&[("shop", "web"), ("shop", "worker"), ("shop", "busy")]);
        let start = Instant::now();
        // The first sweep runs, the next within the interval does not.
        assert!(limits.sweep(start, &known).is_empty());
        limits.admit(&web, Signal::Records, BURST + 2, start, wall(0));
        limits.admit(&worker, Signal::Records, 1, start, wall(0));
        limits.admit(&gone, Signal::Spans, BURST + 7, start, wall(0));
        let _in_flight = limits.enter(&busy, start).unwrap();
        assert!(
            limits
                .sweep(start + Duration::from_secs(1), &known)
                .is_empty()
        );

        // A minute on, both open gaps end.
        let minute = start + OtlpLimits::GAP_MAX;
        let mut ended = limits.sweep(minute, &known);
        ended.sort_by(|a, b| a.identity.cmp(&b.identity));
        assert_eq!(ended.len(), 2);
        assert_eq!((&ended[0].identity, ended[0].gap.dropped), (&gone, 7));
        assert_eq!((&ended[1].identity, ended[1].gap.dropped), (&web, 2));

        // Idle: the service without drops goes, the removed one goes with its counts, the one
        // with drops stays for its counts, the one with a request in flight stays.
        let idle = start + OtlpLimits::IDLE;
        assert!(limits.sweep(idle, &known).is_empty());
        let mut kept: Vec<_> = limits.lock().states.keys().cloned().collect();
        kept.sort();
        assert_eq!(kept, vec![busy, web.clone()]);
        assert_eq!(
            limits.capped(),
            vec![(
                web,
                Capped {
                    records: 2,
                    spans: 0
                }
            )]
        );
    }

    #[test]
    fn an_idle_service_has_its_open_gap_marked_when_it_goes() {
        let limits = OtlpLimits::default();
        let web = identity("shop", "web");
        let start = Instant::now();
        limits.admit(&web, Signal::Records, BURST + 4, start, wall(0));
        // The service is removed before the gap is a minute old: the sweep that lets it go
        // writes its marker.
        let ended = limits.sweep(start + OtlpLimits::IDLE, &known(&[]));
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].gap.dropped, 4);
        assert!(limits.capped().is_empty());
        assert!(limits.lock().states.is_empty());
    }
}
