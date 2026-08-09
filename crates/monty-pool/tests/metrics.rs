//! Pool-level metrics against the real `monty` binary: the instruments a host
//! sees for worker lifecycle, checkout saturation and session outcomes.
//!
//! The turn-level state machine is unit-tested in `src/metrics.rs`; what needs
//! a real worker is the plumbing — that every path which spawns, hands out or
//! discards a worker reaches the adapter.

#![cfg(feature = "telemetry")]

use std::{
    collections::HashMap,
    env,
    future::ready,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, Once},
    time::Duration,
};

use monty_pool::{
    Pool, PoolConfig, PoolError, PrintFuture, ReplConfig,
    telemetry_adapter::{Measurement, MetricKind, MetricValue, TelemetryAdapter, configure_telemetry_adapter},
};
use monty_proto::pb;
use monty_types::PrintStream;
use opentelemetry::trace::{SpanId, TraceId};
use opentelemetry_sdk::{logs::SdkLogRecord, trace::SpanData};

/// An adapter that keeps every measurement instead of exporting it.
#[derive(Default)]
struct Capture(Mutex<Vec<Recorded>>);

/// One captured measurement, with its attributes flattened to strings.
#[derive(Clone, Debug)]
struct Recorded {
    kind: MetricKind,
    name: String,
    value: MetricValue,
    attributes: HashMap<String, String>,
}

impl TelemetryAdapter for Capture {
    fn start_span(&self, _: &SpanData) -> bool {
        true
    }
    fn end_span(&self, _: &SpanData) -> bool {
        true
    }
    fn emit_log(&self, _: SpanId, _: &SdkLogRecord) -> bool {
        true
    }
    fn disable_root(&self, _: TraceId, _: SpanId) {}
    fn record_metric(&self, measurement: &Measurement<'_>) {
        self.0.lock().unwrap().push(Recorded {
            kind: measurement.kind,
            name: measurement.name.to_owned(),
            value: measurement.value,
            attributes: measurement
                .attributes
                .iter()
                .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                .collect(),
        });
    }
}

impl Capture {
    /// Every measurement recorded under `name`.
    fn named(&self, name: &str) -> Vec<Recorded> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|recorded| recorded.name == name)
            .cloned()
            .collect()
    }

    /// The most recent measurement recorded under `name` with `key = value`.
    /// Gauges are re-stated on every pool change, so the last one is the
    /// current value; counters and histograms are asserted on by count.
    #[track_caller]
    fn last(&self, name: &str, key: &str, value: &str) -> Recorded {
        self.named(name)
            .into_iter()
            .rfind(|recorded| recorded.attributes.get(key).is_some_and(|found| found == value))
            .unwrap_or_else(|| panic!("no {name} recorded with {key}={value}"))
    }

    /// Whether anything was recorded under `name`.
    fn has(&self, name: &str) -> bool {
        !self.named(name).is_empty()
    }
}

/// A pool whose metrics land in the returned capture.
async fn pool_with_metrics(mut config: PoolConfig) -> (Pool, Arc<Capture>) {
    let capture = Arc::new(Capture::default());
    let handle = configure_telemetry_adapter(Arc::clone(&capture) as Arc<dyn TelemetryAdapter>).expect("configure");
    config.metrics = Some(handle.metrics());
    let pool = Pool::new(config).await.expect("pool");
    (pool, capture)
}

/// A session that runs one snippet: the common shape a host's metrics cover.
#[tokio::test]
async fn a_feed_records_the_pool_and_turn_instruments() {
    let mut config = PoolConfig::subprocess(monty_binary());
    config.min_processes = 1;
    config.max_processes = 2;
    let (pool, capture) = pool_with_metrics(config).await;

    // pre-warming publishes the gauge and the spawn cost before any checkout
    assert_eq!(
        capture.last("monty.pool.workers", "state", "idle").value,
        MetricValue::I64(1)
    );
    assert_eq!(
        capture.last("monty.pool.workers", "state", "busy").value,
        MetricValue::I64(0)
    );
    let spawn = capture.last("monty.pool.worker.spawn.duration", "outcome", "ok");
    assert_eq!(spawn.kind, MetricKind::Histogram);
    assert_eq!(
        spawn.attributes.get("transport").map(String::as_str),
        Some("subprocess")
    );

    let mut checkout = pool.checkout(&ReplConfig::default()).await.expect("checkout");
    checkout
        .feed("print('hi')\n1 + 1", vec![], vec![], false, &mut no_print)
        .await
        .expect("feed");
    checkout.finish().await.expect("finish");

    // the pre-warmed worker was reused, so no waiting and no second spawn
    assert_eq!(capture.named("monty.pool.checkout.wait").len(), 1);
    capture.last("monty.pool.checkout.wait", "outcome", "idle");
    assert_eq!(capture.named("monty.pool.worker.spawn.duration").len(), 1);

    capture.last("monty.run.duration", "outcome", "complete");
    capture.last("monty.pool.session.duration", "outcome", "ok");
    capture.last("monty.turn.duration", "turn", "configure");
    capture.last("monty.turn.duration", "turn", "reset");
    assert!(capture.has("monty.run.execution_time"));
    // `print` writes 3 bytes including its newline
    assert_eq!(
        capture.last("monty.print.bytes", "stream", "stdout").value,
        MetricValue::I64(3)
    );
    assert!(capture.has("monty.wire.frame.bytes"));
    capture.last("monty.wire.frame.bytes", "direction", "received");

    // finishing returns the worker to the pool rather than ending it
    assert!(!capture.has("monty.pool.worker.terminated"));
    pool.close().await;
    assert_eq!(
        capture.last("monty.pool.worker.terminated", "reason", "closed").value,
        MetricValue::I64(1)
    );
}

/// A checkout that cannot be served, and a session abandoned rather than
/// finished — the two failure shapes a host alerts on.
#[tokio::test]
async fn saturation_and_abandonment_are_recorded() {
    let mut config = PoolConfig::subprocess(monty_binary());
    config.min_processes = 1;
    config.max_processes = 1;
    config.checkout_timeout = Some(Duration::from_millis(50));
    let (pool, capture) = pool_with_metrics(config).await;

    let held = pool.checkout(&ReplConfig::default()).await.expect("checkout");
    let blocked = pool.checkout(&ReplConfig::default()).await.err();
    assert!(matches!(blocked, Some(PoolError::Exhausted)), "{blocked:?}");
    capture.last("monty.pool.checkout.wait", "outcome", "exhausted");

    // dropping without `finish` kills the worker: the session ended, and the
    // worker left the pool
    drop(held);
    capture.last("monty.pool.session.duration", "outcome", "abandoned");
    capture.last("monty.pool.worker.terminated", "reason", "abandoned");
    assert_eq!(
        capture.last("monty.pool.workers", "state", "busy").value,
        MetricValue::I64(0)
    );
}

/// A worker that dies during a turn: the teardown path counts its termination
/// from the capacity guard, so the count survives a caller cancelling the turn
/// future while the reap is still running.
#[tokio::test]
async fn a_crashed_worker_is_counted_as_it_is_torn_down() {
    let mut config = PoolConfig::subprocess(monty_binary());
    config.min_processes = 1;
    config.max_processes = 1;
    let (pool, capture) = pool_with_metrics(config).await;

    // an expression deep enough to overflow the child's stack, which aborts it
    // mid-turn (the same trick `pool_test.rs` uses, and portable unlike a kill)
    let mut code = String::with_capacity(300_002);
    code.push('a');
    for _ in 0..150_000 {
        code.push_str(".x");
    }
    let mut checkout = pool.checkout(&ReplConfig::default()).await.expect("checkout");
    let err = checkout
        .feed(&code, vec![], vec![], false, &mut no_print)
        .await
        .expect_err("the worker should have died");
    assert!(matches!(err, PoolError::Crashed { .. }), "{err}");
    drop(checkout);

    // exactly once: the crash is counted where the slot is released, not at
    // both the teardown and the drop that follows it
    assert_eq!(capture.named("monty.pool.worker.terminated").len(), 1);
    capture.last("monty.pool.worker.terminated", "reason", "crash");
    assert_eq!(
        capture.last("monty.pool.workers", "state", "busy").value,
        MetricValue::I64(0)
    );
}

/// A relay driving wire-level turns gets the same instrumentation as a typed
/// caller: the state machine lives on the worker, below the checkout's
/// typed/raw split, so neither path can be instrumented and the other not.
#[tokio::test]
async fn raw_turns_are_instrumented_like_typed_ones() {
    let mut config = PoolConfig::subprocess(monty_binary());
    config.min_processes = 1;
    config.max_processes = 1;
    let (pool, capture) = pool_with_metrics(config).await;

    let mut checkout = pool.checkout(&ReplConfig::default()).await.expect("checkout");
    let mut on_event = |_: &pb::ChildEvent| Box::pin(ready(())) as PrintFuture;
    let feed = pb::ParentRequest {
        kind: Some(pb::parent_request::Kind::Feed(pb::Feed {
            code: "print('hi')\n6 * 7".to_owned(),
            inputs: vec![],
            skip_type_check: false,
        })),
        ..pb::ParentRequest::default()
    };
    let event = checkout.turn_raw(&feed, &mut on_event).await.expect("raw feed");
    assert!(
        matches!(event.kind, Some(pb::child_event::Kind::Complete(_))),
        "{event:?}"
    );
    checkout.finish().await.expect("finish");

    capture.last("monty.run.duration", "outcome", "complete");
    assert!(capture.has("monty.run.execution_time"));
    assert_eq!(
        capture.last("monty.print.bytes", "stream", "stdout").value,
        MetricValue::I64(3)
    );
}

/// A discard-everything print callback, coercible to `OnPrint` at each callsite.
fn no_print(_: PrintStream, _: &str) -> PrintFuture {
    Box::pin(ready(()))
}

/// Locates (building once if needed) the `monty` CLI binary for tests.
fn monty_binary() -> PathBuf {
    static BUILD: Once = Once::new();
    if let Ok(path) = env::var("MONTY_TEST_BIN") {
        return PathBuf::from(path);
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_owned();
    let target = env::var_os("CARGO_TARGET_DIR").map_or_else(|| workspace.join("target"), PathBuf::from);
    let path = target.join("debug").join(format!("monty{}", env::consts::EXE_SUFFIX));
    BUILD.call_once(|| {
        if !path.exists() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "monty-runtime"])
                .status()
                .expect("failed to run cargo build -p monty-runtime");
            assert!(status.success(), "building the monty binary failed");
        }
    });
    assert!(path.exists(), "monty binary missing at {}", path.display());
    path
}
