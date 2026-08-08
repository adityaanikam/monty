//! Aggregate metrics for pool health and turn latency — what the spans in
//! [`crate::telemetry`] cannot answer: is the fleet saturated, how often do
//! workers die, how much of its duration budget does a typical run use.
//!
//! Recorded for *every* checkout, not only those a host gave a
//! [`TelemetryContext`](crate::telemetry_adapter::TelemetryContext) to: an
//! aggregate covering only traced sessions would mislead. The host owns the
//! instruments and the aggregation; each measurement is pushed to
//! [`TelemetryAdapter::record_metric`] with the name, unit and description its
//! SDK needs to create the instrument on first use.
//!
//! **Attribute cardinality is a hard constraint.** Sandboxed code chooses
//! function names and exception classes, and one time series per value would
//! take a metrics backend down, so only closed sets reach attributes — see
//! [`exception_label`] and the resolved-only rule in
//! [`TurnMetrics::close_suspension`].

use std::{
    collections::HashMap,
    fmt,
    str::FromStr,
    sync::{Arc, PoisonError, RwLock},
    time::{Duration, Instant},
};

use logfire::{ExponentialHistogram, Logfire};
use monty_proto::{pb, pb::os_call::Call};
use monty_types::{ExcType, MontyObject};
use opentelemetry::{
    KeyValue,
    metrics::{Counter, Gauge},
};

use crate::telemetry_adapter::TelemetryAdapter;

/// Records measurements into the host's metrics SDK.
///
/// Put on [`PoolConfig::metrics`](crate::PoolConfig::metrics); cheap to clone
/// (one `Arc`). Comes from
/// [`TelemetryAdapterHandle::metrics`](crate::telemetry_adapter::TelemetryAdapterHandle::metrics)
/// for a foreign-SDK host, or [`Metrics::for_logfire`] for a Rust one.
#[derive(Clone)]
pub struct Metrics(Arc<Sink>);

/// Where measurements go, which is what separates a host that owns an OTel SDK
/// from one that only owns a bridge to a foreign one.
enum Sink {
    /// Pushed one at a time to a host adapter, which owns the instruments.
    Adapter(Arc<dyn TelemetryAdapter>),
    /// Recorded into instruments of our own, built from a Rust host's meter.
    /// Boxed: the instrument map dwarfs the adapter pointer beside it.
    Logfire(Box<Instruments>),
}

impl Metrics {
    /// Wraps the adapter a configured pipeline delivers measurements to.
    pub(crate) fn new(adapter: Arc<dyn TelemetryAdapter>) -> Self {
        Self(Arc::new(Sink::Adapter(adapter)))
    }

    /// Records into a Rust host's own `Logfire`, with no adapter in between.
    ///
    /// The adapter exists to hand measurements to a *foreign* SDK. A Rust host
    /// already has a meter provider, so it gets real instruments — aggregated
    /// in-process, with exponential histogram buckets — instead of implementing
    /// [`TelemetryAdapter`] to receive its own measurements. The span-side
    /// equivalent is `TelemetryContext::for_logfire`.
    #[must_use]
    pub fn for_logfire(logfire: Logfire) -> Self {
        Self(Arc::new(Sink::Logfire(Box::new(Instruments::new(logfire)))))
    }

    /// Live worker counts, re-stated after every change to the pool's state.
    ///
    /// A gauge rather than an up/down counter: a decrement missed on one error
    /// path would skew an up/down counter for the process's whole life, while
    /// a gauge is corrected by the next observation.
    pub(crate) fn workers(&self, idle: usize, busy: usize) {
        for (state, count) in [("idle", idle), ("busy", busy)] {
            self.record(
                &WORKERS,
                MetricValue::I64(i64::try_from(count).unwrap_or(i64::MAX)),
                &[KeyValue::new("state", state)],
            );
        }
    }

    /// Time [`Pool::checkout`](crate::Pool::checkout) spent obtaining a worker.
    ///
    /// The saturation signal: a non-zero `waited` tail means `max_processes`
    /// is below what the workload needs, and `exhausted` is already a
    /// user-visible failure.
    pub(crate) fn checkout_wait(&self, elapsed: Duration, outcome: &'static str) {
        self.record(
            &CHECKOUT_WAIT,
            MetricValue::seconds(elapsed),
            &[KeyValue::new("outcome", outcome)],
        );
    }

    /// Cost of spawning a subprocess worker or dialing a remote one.
    pub(crate) fn worker_spawn(&self, elapsed: Duration, transport: &'static str, outcome: &'static str) {
        self.record(
            &SPAWN_DURATION,
            MetricValue::seconds(elapsed),
            &[KeyValue::new("transport", transport), KeyValue::new("outcome", outcome)],
        );
    }

    /// One worker leaving the pool, by why it left. `crash` versus `oom`
    /// separates "our bug" from "the sandboxed code asked for too much".
    pub(crate) fn worker_terminated(&self, reason: &'static str) {
        self.record(
            &WORKER_TERMINATED,
            MetricValue::I64(1),
            &[KeyValue::new("reason", reason)],
        );
    }

    /// Lifetime of one checkout, from `Configure` to `finish` (or to the drop
    /// that abandoned it). With the checkout rate this sizes the pool.
    pub(crate) fn session_duration(&self, elapsed: Duration, outcome: &'static str) {
        self.record(
            &SESSION_DURATION,
            MetricValue::seconds(elapsed),
            &[KeyValue::new("outcome", outcome)],
        );
    }

    /// Hands one measurement to whichever sink this handle was built with.
    fn record(&self, instrument: &Instrument, value: MetricValue, attributes: &[KeyValue]) {
        match &*self.0 {
            Sink::Adapter(adapter) => adapter.record_metric(&Measurement {
                kind: instrument.kind,
                name: instrument.name,
                unit: instrument.unit,
                description: instrument.description,
                value,
                attributes,
            }),
            Sink::Logfire(instruments) => instruments.record(instrument, value, attributes),
        }
    }
}

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Metrics")
    }
}

/// Instruments built from a Rust host's meter, on first use of each.
///
/// Lazy rather than a registered list built up front: an instrument then needs
/// no bookkeeping beyond its own [`Instrument`], so one that only a rare code
/// path records can never be forgotten and silently dropped. The read lock is
/// the steady state — building happens at most once per instrument per pool.
struct Instruments {
    logfire: Logfire,
    built: RwLock<HashMap<&'static str, Handle>>,
}

impl Instruments {
    fn new(logfire: Logfire) -> Self {
        Self {
            logfire,
            built: RwLock::new(HashMap::new()),
        }
    }

    /// Records into `instrument`'s handle, building it if this is its first use.
    fn record(&self, instrument: &Instrument, value: MetricValue, attributes: &[KeyValue]) {
        {
            let built = self.built.read().unwrap_or_else(PoisonError::into_inner);
            if let Some(handle) = built.get(instrument.name) {
                handle.record(value, attributes);
                return;
            }
        }
        // built under the write lock, not before it: two handles for one
        // exponential histogram would share a scale registration, and dropping
        // the loser would deregister the winner's scale
        let mut built = self.built.write().unwrap_or_else(PoisonError::into_inner);
        built
            .entry(instrument.name)
            .or_insert_with(|| self.build(instrument))
            .record(value, attributes);
    }

    /// Creates one instrument on the host's meter.
    fn build(&self, instrument: &Instrument) -> Handle {
        let metrics = self.logfire.metrics();
        match instrument.kind {
            MetricKind::Counter => Handle::Counter(
                metrics
                    .u64_counter(instrument.name)
                    .with_unit(instrument.unit)
                    .with_description(instrument.description)
                    .build(),
            ),
            MetricKind::Gauge => Handle::Gauge(
                metrics
                    .i64_gauge(instrument.name)
                    .with_unit(instrument.unit)
                    .with_description(instrument.description)
                    .build(),
            ),
            // exponential, not the SDK's default buckets: those are scaled for
            // seconds-long requests and would put every monty turn in the
            // first bucket
            MetricKind::Histogram => Handle::Histogram(
                metrics
                    .f64_exponential_histogram(instrument.name, MAX_HISTOGRAM_SCALE)
                    .with_unit(instrument.unit)
                    .with_description(instrument.description)
                    .build(),
            ),
        }
    }
}

/// Upper bound on exponential histogram resolution; the SDK downscales from
/// here as a distribution widens, so this only says "as fine as OTel allows".
const MAX_HISTOGRAM_SCALE: i8 = 20;

/// One built instrument, typed by the kind that produced it.
enum Handle {
    Counter(Counter<u64>),
    Gauge(Gauge<i64>),
    Histogram(ExponentialHistogram<f64>),
}

impl Handle {
    fn record(&self, value: MetricValue, attributes: &[KeyValue]) {
        match self {
            Self::Counter(counter) => counter.add(value.as_u64(), attributes),
            Self::Gauge(gauge) => gauge.record(value.as_i64(), attributes),
            Self::Histogram(histogram) => histogram.record(value.as_f64(), attributes),
        }
    }
}

/// One measurement, with everything the host needs to create the instrument it
/// belongs to: `kind`, `unit` and `description` are constant for a given
/// [`Self::name`], so a host can create it on first sight and cache it.
pub struct Measurement<'a> {
    /// Which kind of instrument records this measurement.
    pub kind: MetricKind,
    /// Dotted instrument name, e.g. `monty.pool.checkout.wait`.
    pub name: &'static str,
    /// UCUM unit: `s`, `By`, `1`, or a `{thing}` annotation for counts.
    pub unit: &'static str,
    /// One-line description of what the instrument measures.
    pub description: &'static str,
    /// The measured value.
    pub value: MetricValue,
    /// Dimensions to record it under; always a closed set of values.
    pub attributes: &'a [KeyValue],
}

/// The kind of instrument a measurement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonic sum: the value is an increment.
    Counter,
    /// Last-value-wins observation: the value is the current absolute state.
    Gauge,
    /// Distribution: the value is one sample.
    Histogram,
}

/// A measured value, integral for counts and byte sizes, floating for
/// durations and ratios.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricValue {
    /// An integral count.
    I64(i64),
    /// A duration in seconds, or a ratio.
    F64(f64),
}

impl MetricValue {
    /// A duration as fractional seconds, the unit OTel durations use.
    fn seconds(duration: Duration) -> Self {
        Self::F64(duration.as_secs_f64())
    }

    /// A byte count, saturating rather than wrapping on absurd sizes.
    fn bytes(len: usize) -> Self {
        Self::I64(i64::try_from(len).unwrap_or(i64::MAX))
    }

    /// The value as a histogram sample.
    fn as_f64(self) -> f64 {
        match self {
            Self::I64(value) => value as f64,
            Self::F64(value) => value,
        }
    }

    /// The value as a gauge observation. Only durations and ratios are `F64`,
    /// and no gauge records either, so the saturating cast never runs.
    #[expect(clippy::cast_possible_truncation, reason = "float→int casts saturate")]
    fn as_i64(self) -> i64 {
        match self {
            Self::I64(value) => value,
            Self::F64(value) => value as i64,
        }
    }

    /// The value as a counter increment; a negative one would be a bug in a
    /// caller, and saturates to zero rather than wrapping into a huge count.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "float→int casts saturate, including negatives to zero"
    )]
    fn as_u64(self) -> u64 {
        match self {
            Self::I64(value) => u64::try_from(value).unwrap_or(0),
            Self::F64(value) => value as u64,
        }
    }
}

/// The `outcome` attribute for an operation that either worked or did not.
pub(crate) const fn outcome(ok: bool) -> &'static str {
    if ok { "ok" } else { "error" }
}

/// The constant half of an instrument, shared by every measurement it records.
struct Instrument {
    kind: MetricKind,
    name: &'static str,
    unit: &'static str,
    description: &'static str,
}

/// Records one worker's turns, mirroring the protocol the way
/// [`crate::telemetry::Recorder`] mirrors it for spans.
///
/// A separate state machine rather than a branch inside that recorder, because
/// metrics must also run for untraced checkouts. Lives on the
/// [`Worker`](crate::worker::Worker), which sees every request and event.
pub(crate) struct TurnMetrics {
    metrics: Metrics,
    /// When the in-flight feed started; `None` between feeds, and for a feed
    /// restored mid-suspension (whose start this process never saw).
    feed: Option<Instant>,
    /// The suspension the feed is blocked on, timing the host round-trip.
    pending: Option<Suspension>,
    /// The in-flight housekeeping turn: what to label it, and when it started.
    turn: Option<(&'static str, Instant)>,
    /// Cumulative sandbox execution time as last reported, so each run
    /// contributes its own delta instead of the session total.
    reported_micros: u64,
}

impl TurnMetrics {
    /// Creates the per-worker recorder, which outlives the checkouts it serves.
    pub(crate) const fn new(metrics: Metrics) -> Self {
        Self {
            metrics,
            feed: None,
            pending: None,
            turn: None,
            reported_micros: 0,
        }
    }

    /// Records the size of one frame that reached the wire.
    pub(crate) fn frame(&self, direction: &'static str, len: usize) {
        self.metrics.record(
            &FRAME_BYTES,
            MetricValue::bytes(len),
            &[KeyValue::new("direction", direction)],
        );
    }

    /// Starts timing one turn; called once the request is on the wire.
    ///
    /// The resume family opens nothing — it *answers* the open suspension, so
    /// it closes that instead, which is what makes the recorded duration the
    /// host round-trip.
    pub(crate) fn begin_turn(&mut self, request: &pb::ParentRequest) {
        let now = Instant::now();
        match &request.kind {
            // a new session on this worker; its execution clock restarts, so
            // the delta ratchet has to as well
            Some(pb::parent_request::Kind::Configure(_)) => {
                self.feed = None;
                self.pending = None;
                self.turn = Some(("configure", now));
                self.reported_micros = 0;
            }
            Some(pb::parent_request::Kind::Feed(_)) => {
                self.pending = None;
                self.feed = Some(now);
            }
            Some(pb::parent_request::Kind::Load(_)) => {
                // a load adopts the dumped session's clock, which may be ahead
                // of or behind this worker's; the first reply re-bases it
                self.reported_micros = 0;
                self.turn = Some(("load", now));
            }
            Some(pb::parent_request::Kind::Dump(_)) => self.turn = Some(("dump", now)),
            Some(pb::parent_request::Kind::InstallDependencies(_)) => {
                self.turn = Some(("install_dependencies", now));
            }
            Some(pb::parent_request::Kind::Reset(_)) => {
                self.feed = None;
                self.pending = None;
                self.turn = Some(("reset", now));
            }
            // no reply is awaited, so nothing is left open to close
            Some(pb::parent_request::Kind::Shutdown(_)) => {
                self.feed = None;
                self.pending = None;
                self.turn = None;
            }
            Some(pb::parent_request::Kind::ResumeCall(r)) => {
                let (outcome, value) = ext_result(r.result.as_ref());
                self.close_suspension(outcome, value);
            }
            Some(pb::parent_request::Kind::ResumeNameLookup(r)) => {
                let outcome = match r.kind {
                    Some(pb::resume_name_lookup::Kind::Value(_)) => "value",
                    Some(pb::resume_name_lookup::Kind::Undefined(_)) => "undefined",
                    None => "missing",
                };
                self.close_suspension(outcome, None);
            }
            Some(pb::parent_request::Kind::ResumeFutures(_)) => self.close_suspension("resolved", None),
            None => {}
        }
    }

    /// Records one event from the worker: a suspension opens the pending
    /// round-trip, a turn-ending event closes the run or housekeeping turn.
    pub(crate) fn event(&mut self, event: &pb::ChildEvent) {
        match &event.kind {
            Some(pb::child_event::Kind::Print(p)) => self.metrics.record(
                &PRINT_BYTES,
                MetricValue::bytes(p.text.len()),
                &[KeyValue::new("stream", print_stream(p.stream))],
            ),
            Some(pb::child_event::Kind::FunctionCall(c)) => {
                self.suspend(
                    "function",
                    SuspensionKind::Function {
                        name: c.function_name.clone(),
                    },
                );
            }
            Some(pb::child_event::Kind::OsCall(c)) => {
                let (function, reads) = os_call(c.call.as_ref());
                // a write's payload is in the request the sandbox made, so its
                // size is known now; a read's arrives with the host's answer
                if let Some(written) = os_call_written(c.call.as_ref()) {
                    self.io_bytes("write", written);
                }
                self.suspend("os", SuspensionKind::Os { function, reads });
            }
            Some(pb::child_event::Kind::NameLookup(_)) => self.suspend("name_lookup", SuspensionKind::Plain),
            Some(pb::child_event::Kind::ResolveFutures(_)) => self.suspend("futures", SuspensionKind::Plain),
            Some(pb::child_event::Kind::Complete(_)) => self.end_run("complete", event),
            Some(pb::child_event::Kind::Error(e)) => {
                self.metrics.record(
                    &ERRORS,
                    MetricValue::I64(1),
                    &[KeyValue::new(
                        "exc_type",
                        exception_label(e.exception.as_ref().map(|exc| exc.exc_type.as_str())),
                    )],
                );
                // an error answering a `Dump` leaves the feed suspended and
                // resumable, so only the dump turn ends here
                if matches!(self.turn, Some(("dump", _))) {
                    self.end_turn("error");
                } else {
                    self.end_run("error", event);
                }
            }
            Some(pb::child_event::Kind::TypingError(_)) => self.end_run("typing_error", event),
            Some(pb::child_event::Kind::DumpResult(d)) => {
                self.metrics.record(
                    &SNAPSHOT_BYTES,
                    MetricValue::bytes(d.state.len()),
                    &[KeyValue::new("op", "dump")],
                );
                self.end_turn("ok");
            }
            Some(pb::child_event::Kind::Ok(_)) => self.end_turn("ok"),
            // the worker is about to exit; the pool counts the termination
            Some(pb::child_event::Kind::FatalError(_) | pb::child_event::Kind::Shutdown(_)) | None => {}
        }
    }

    /// Counts one suspension and starts timing the host's answer.
    fn suspend(&mut self, kind: &'static str, suspension: SuspensionKind) {
        self.metrics
            .record(&SUSPENSIONS, MetricValue::I64(1), &[KeyValue::new("kind", kind)]);
        self.pending = Some(Suspension {
            start: Instant::now(),
            kind: suspension,
            label: kind,
        });
    }

    /// Records the round-trip the answering resume just closed.
    ///
    /// The called function's name is recorded **only when the host resolved
    /// it**: an unresolved name is whatever the sandboxed code wrote, so
    /// recording those would let a script mint a time series per call.
    fn close_suspension(&mut self, outcome: &'static str, value: Option<&MontyObject>) {
        let Some(suspension) = self.pending.take() else {
            return;
        };
        let elapsed = MetricValue::seconds(suspension.start.elapsed());
        match suspension.kind {
            SuspensionKind::Os { function, reads } => {
                if reads && let Some(read) = value_len(value) {
                    self.io_bytes("read", read);
                }
                self.metrics.record(
                    &OS_CALL,
                    elapsed,
                    &[KeyValue::new("function", function), KeyValue::new("outcome", outcome)],
                );
            }
            SuspensionKind::Function { name } => {
                let mut attributes = vec![
                    KeyValue::new("kind", suspension.label),
                    KeyValue::new("outcome", outcome),
                ];
                if outcome != "not_found" {
                    attributes.push(KeyValue::new("function", name));
                }
                self.metrics.record(&HOST_CALL, elapsed, &attributes);
            }
            SuspensionKind::Plain => self.metrics.record(
                &HOST_CALL,
                elapsed,
                &[
                    KeyValue::new("kind", suspension.label),
                    KeyValue::new("outcome", outcome),
                ],
            ),
        }
    }

    /// Ends an execution turn: its wall time, the sandbox time it consumed,
    /// and how much of any duration budget that leaves.
    ///
    /// A feed restored mid-suspension has no start instant in this process, so
    /// it contributes execution time and budget use but no wall time.
    fn end_run(&mut self, outcome: &'static str, event: &pb::ChildEvent) {
        self.pending = None;
        self.end_turn(outcome);
        if let Some(start) = self.feed.take() {
            self.metrics.record(
                &RUN_DURATION,
                MetricValue::seconds(start.elapsed()),
                &[KeyValue::new("outcome", outcome)],
            );
        }
        // the reported total is cumulative for the session (and never rewinds,
        // even from a worker misreporting it), so this run's cost is the delta
        let total = event.total_execution_micros;
        let delta = total.saturating_sub(self.reported_micros);
        self.reported_micros = self.reported_micros.max(total);
        self.metrics
            .record(&RUN_EXECUTION, MetricValue::seconds(Duration::from_micros(delta)), &[]);
        if let Some(budget) = event.max_duration_micros.filter(|budget| *budget > 0) {
            let used = total as f64 / budget as f64;
            self.metrics.record(&RUN_BUDGET, MetricValue::F64(used), &[]);
        }
    }

    /// Ends the open housekeeping turn, if there is one.
    fn end_turn(&mut self, outcome: &'static str) {
        if let Some((turn, start)) = self.turn.take() {
            self.metrics.record(
                &TURN_DURATION,
                MetricValue::seconds(start.elapsed()),
                &[KeyValue::new("turn", turn), KeyValue::new("outcome", outcome)],
            );
        }
    }

    /// Counts bytes crossing a mount in one direction.
    fn io_bytes(&self, direction: &'static str, len: usize) {
        self.metrics.record(
            &OS_IO,
            MetricValue::bytes(len),
            &[KeyValue::new("direction", direction)],
        );
    }
}

/// The suspension a feed is blocked on while the host answers it.
struct Suspension {
    start: Instant,
    kind: SuspensionKind,
    /// Value of the `kind` attribute this suspension is counted under.
    label: &'static str,
}

/// What a pending suspension needs remembered to record its answer.
enum SuspensionKind {
    /// An external function call, named by the sandboxed code — see the
    /// resolved-only rule in [`TurnMetrics::close_suspension`].
    Function { name: String },
    /// An OS call: its fixed name, and whether the answer carries read bytes.
    Os { function: &'static str, reads: bool },
    /// A name lookup or a futures wait, which need nothing beyond their timing.
    Plain,
}

/// Classifies the host's answer to a call suspension, with the returned value
/// when there is one (so a read's size can be counted).
fn ext_result(result: Option<&pb::ExtFunctionResult>) -> (&'static str, Option<&MontyObject>) {
    match result.and_then(|result| result.kind.as_ref()) {
        Some(pb::ext_function_result::Kind::ReturnValue(v)) => ("value", v.0.as_ref()),
        Some(pb::ext_function_result::Kind::Error(_)) => ("error", None),
        Some(pb::ext_function_result::Kind::Future(_)) => ("future", None),
        Some(pb::ext_function_result::Kind::NotFound(_)) => ("not_found", None),
        Some(pb::ext_function_result::Kind::NotHandled(_)) => ("not_handled", None),
        None => ("missing", None),
    }
}

/// The fixed name of an OS call, and whether its answer carries read bytes.
///
/// The names match the `os call {function}` spans in [`crate::telemetry`];
/// keep the two lists in step.
fn os_call(call: Option<&Call>) -> (&'static str, bool) {
    match call {
        Some(Call::Exists(_)) => ("exists", false),
        Some(Call::IsFile(_)) => ("is_file", false),
        Some(Call::IsDir(_)) => ("is_dir", false),
        Some(Call::IsSymlink(_)) => ("is_symlink", false),
        Some(Call::ReadText(_)) => ("read_text", true),
        Some(Call::ReadBytes(_)) => ("read_bytes", true),
        Some(Call::Stat(_)) => ("stat", false),
        Some(Call::Iterdir(_)) => ("iterdir", false),
        Some(Call::Resolve(_)) => ("resolve", false),
        Some(Call::Absolute(_)) => ("absolute", false),
        Some(Call::Unlink(_)) => ("unlink", false),
        Some(Call::Rmdir(_)) => ("rmdir", false),
        Some(Call::WriteText(_)) => ("write_text", false),
        Some(Call::AppendText(_)) => ("append_text", false),
        Some(Call::WriteBytes(_)) => ("write_bytes", false),
        Some(Call::AppendBytes(_)) => ("append_bytes", false),
        Some(Call::Open(_)) => ("open", false),
        Some(Call::Mkdir(_)) => ("mkdir", false),
        Some(Call::Rename(_)) => ("rename", false),
        Some(Call::Getenv(_)) => ("getenv", false),
        Some(Call::GetEnviron(_)) => ("get_environ", false),
        Some(Call::DateToday(_)) => ("date_today", false),
        Some(Call::DateTimeNow(_)) => ("date_time_now", false),
        None => ("unknown", false),
    }
}

/// Payload size of an OS call that writes, `None` for one that does not.
fn os_call_written(call: Option<&Call>) -> Option<usize> {
    match call {
        Some(Call::WriteText(w) | Call::AppendText(w)) => Some(w.data.len()),
        Some(Call::WriteBytes(w) | Call::AppendBytes(w)) => Some(w.data.len()),
        _ => None,
    }
}

/// Size of a value returned by a reading OS call.
fn value_len(value: Option<&MontyObject>) -> Option<usize> {
    match value? {
        MontyObject::String(s) => Some(s.len()),
        MontyObject::Bytes(b) => Some(b.len()),
        _ => None,
    }
}

/// Bounds `exc_type` to the exception classes the interpreter knows: the
/// sandbox can raise a class of any name, and each distinct name would
/// otherwise become its own time series.
fn exception_label(exc_type: Option<&str>) -> &'static str {
    exc_type
        .and_then(|exc_type| ExcType::from_str(exc_type).ok())
        .map_or("other", Into::into)
}

/// The name of a `PrintStream` enum value.
fn print_stream(stream: i32) -> &'static str {
    match pb::PrintStream::try_from(stream) {
        Ok(pb::PrintStream::Stdout) => "stdout",
        Ok(pb::PrintStream::Stderr) => "stderr",
        _ => "unspecified",
    }
}

/// Live workers, by whether they are serving a checkout.
static WORKERS: Instrument = Instrument {
    kind: MetricKind::Gauge,
    name: "monty.pool.workers",
    unit: "{worker}",
    description: "Live monty workers, by state.",
};

/// Time spent waiting for a worker to check out.
static CHECKOUT_WAIT: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.pool.checkout.wait",
    unit: "s",
    description: "Time spent acquiring a worker for a checkout.",
};

/// Cost of adding a worker to the pool.
static SPAWN_DURATION: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.pool.worker.spawn.duration",
    unit: "s",
    description: "Time to spawn a subprocess worker or dial a remote one.",
};

/// Workers leaving the pool, by why.
static WORKER_TERMINATED: Instrument = Instrument {
    kind: MetricKind::Counter,
    name: "monty.pool.worker.terminated",
    unit: "{worker}",
    description: "Workers discarded by the pool, by reason.",
};

/// Checkout lifetime.
static SESSION_DURATION: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.pool.session.duration",
    unit: "s",
    description: "Lifetime of a checked-out session.",
};

/// Wall time of one execution turn.
static RUN_DURATION: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.run.duration",
    unit: "s",
    description: "Wall time of one feed, including time spent waiting on the host.",
};

/// Sandbox time of one execution turn.
static RUN_EXECUTION: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.run.execution_time",
    unit: "s",
    description: "Sandbox execution time of one feed, excluding host round-trips.",
};

/// Share of the session's duration budget consumed.
static RUN_BUDGET: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.run.duration_budget_used",
    unit: "1",
    description: "Fraction of the session's max_duration consumed after a feed.",
};

/// Wall time of one housekeeping turn.
static TURN_DURATION: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.turn.duration",
    unit: "s",
    description: "Wall time of one non-execution turn, by kind.",
};

/// Suspensions the sandbox raised.
static SUSPENSIONS: Instrument = Instrument {
    kind: MetricKind::Counter,
    name: "monty.run.suspensions",
    unit: "{suspension}",
    description: "Suspensions the sandbox raised for the host to answer, by kind.",
};

/// Host round-trip time.
static HOST_CALL: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.host.call.duration",
    unit: "s",
    description: "Time the host took to answer a suspension.",
};

/// Mount round-trip time.
static OS_CALL: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.os.call.duration",
    unit: "s",
    description: "Time the host took to answer an os call, by function.",
};

/// Bytes crossing a mount.
static OS_IO: Instrument = Instrument {
    kind: MetricKind::Counter,
    name: "monty.os.io.bytes",
    unit: "By",
    description: "Bytes read from and written to mounts.",
};

/// Exceptions that ended a feed.
static ERRORS: Instrument = Instrument {
    kind: MetricKind::Counter,
    name: "monty.errors",
    unit: "{error}",
    description: "Feeds ended by an exception, by exception type.",
};

/// Session dump size.
static SNAPSHOT_BYTES: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.snapshot.bytes",
    unit: "By",
    description: "Size of a session dump.",
};

/// Sandbox output volume.
static PRINT_BYTES: Instrument = Instrument {
    kind: MetricKind::Counter,
    name: "monty.print.bytes",
    unit: "By",
    description: "Bytes the sandbox printed, by stream.",
};

/// Protocol frame size.
static FRAME_BYTES: Instrument = Instrument {
    kind: MetricKind::Histogram,
    name: "monty.wire.frame.bytes",
    unit: "By",
    description: "Size of one protocol frame, by direction.",
};

// tests live here rather than in `tests/` because `TurnMetrics` is
// crate-private: recording is a side effect of the worker, not part of the
// pool's public API. `tests/metrics.rs` covers the pool-level instruments,
// which a public `Pool` does emit.
#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use logfire::config::MetricsOptions;
    use monty_proto::{WireFunctionCall, pb, pb::os_call::Call};
    use monty_types::MontyObject;
    use opentelemetry::trace::{SpanId, TraceId};
    use opentelemetry_sdk::{
        logs::SdkLogRecord,
        metrics::{
            InMemoryMetricExporter, PeriodicReader,
            data::{AggregatedMetrics, MetricData},
        },
        trace::SpanData,
    };

    use super::{Measurement, MetricValue, Metrics, TurnMetrics};
    use crate::telemetry_adapter::TelemetryAdapter;

    /// An adapter that keeps every measurement instead of exporting it.
    #[derive(Default)]
    struct Capture(Mutex<Vec<Recorded>>);

    /// One captured measurement, with its attributes flattened to strings.
    struct Recorded {
        name: &'static str,
        value: MetricValue,
        attributes: Vec<(String, String)>,
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
                name: measurement.name,
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
        /// The attributes of every measurement recorded under `name`.
        fn attributes(&self, name: &str) -> Vec<Vec<(String, String)>> {
            self.select(name, |recorded| recorded.attributes.clone())
        }

        /// The values of every measurement recorded under `name`.
        fn values(&self, name: &str) -> Vec<MetricValue> {
            self.select(name, |recorded| recorded.value)
        }

        fn select<T>(&self, name: &str, map: impl Fn(&Recorded) -> T) -> Vec<T> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|recorded| recorded.name == name)
                .map(map)
                .collect()
        }
    }

    /// A recorder writing into a fresh capture.
    fn recorder() -> (TurnMetrics, Arc<Capture>) {
        let capture = Arc::new(Capture::default());
        let metrics = Metrics::new(Arc::clone(&capture) as Arc<dyn TelemetryAdapter>);
        (TurnMetrics::new(metrics), capture)
    }

    fn request(kind: pb::parent_request::Kind) -> pb::ParentRequest {
        pb::ParentRequest {
            kind: Some(kind),
            trace_parent: None,
        }
    }

    fn event(kind: pb::child_event::Kind) -> pb::ChildEvent {
        pb::ChildEvent {
            kind: Some(kind),
            total_execution_micros: 0,
            max_duration_micros: None,
            restored_script_name: None,
        }
    }

    fn feed() -> pb::ParentRequest {
        request(pb::parent_request::Kind::Feed(pb::Feed {
            code: "double(2)".to_owned(),
            inputs: vec![],
            skip_type_check: false,
        }))
    }

    fn call_event(function_name: &str) -> pb::ChildEvent {
        event(pb::child_event::Kind::FunctionCall(WireFunctionCall {
            function_name: function_name.to_owned(),
            args: vec![],
            kwargs: vec![],
            call_id: 1,
            method_call: false,
        }))
    }

    fn resume_call(kind: pb::ext_function_result::Kind) -> pb::ParentRequest {
        request(pb::parent_request::Kind::ResumeCall(pb::ResumeCall {
            call_id: 1,
            result: Some(pb::ExtFunctionResult { kind: Some(kind) }),
        }))
    }

    fn attribute<'a>(attributes: &'a [(String, String)], key: &str) -> Option<&'a str> {
        attributes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value.as_str())
    }

    /// One feed with one host call: the suspension is counted, the round-trip
    /// timed on its own, and the run recorded when the completion arrives.
    #[test]
    fn a_feed_records_its_run_and_its_round_trip() {
        let (mut metrics, capture) = recorder();
        metrics.begin_turn(&feed());
        metrics.event(&call_event("double"));
        metrics.begin_turn(&resume_call(pb::ext_function_result::Kind::ReturnValue(
            MontyObject::Int(4).into(),
        )));
        metrics.event(&event(pb::child_event::Kind::Complete(pb::Complete {
            value: Some(MontyObject::Int(4).into()),
        })));

        assert_eq!(
            capture.attributes("monty.run.suspensions"),
            [[("kind".to_owned(), "function".to_owned())]]
        );
        let calls = capture.attributes("monty.host.call.duration");
        assert_eq!(attribute(&calls[0], "function"), Some("double"));
        assert_eq!(attribute(&calls[0], "outcome"), Some("value"));
        assert_eq!(
            capture.attributes("monty.run.duration"),
            [[("outcome".to_owned(), "complete".to_owned())]]
        );
        // no budget was configured, so only the execution time is recorded
        assert_eq!(capture.values("monty.run.execution_time").len(), 1);
        assert!(capture.values("monty.run.duration_budget_used").is_empty());
    }

    /// A name the host could not resolve is chosen by the sandboxed code, so it
    /// must not become an attribute value (see [`TurnMetrics::close_suspension`]).
    #[test]
    fn unresolved_function_names_are_not_recorded() {
        let (mut metrics, capture) = recorder();
        metrics.begin_turn(&feed());
        metrics.event(&call_event("attacker_chosen_name"));
        metrics.begin_turn(&resume_call(pb::ext_function_result::Kind::NotFound(
            "attacker_chosen_name".to_owned(),
        )));

        let calls = capture.attributes("monty.host.call.duration");
        assert_eq!(attribute(&calls[0], "function"), None);
        assert_eq!(attribute(&calls[0], "outcome"), Some("not_found"));
    }

    /// Mount traffic is counted in both directions: a write from the request
    /// the sandbox made, a read from the answer the host gave.
    #[test]
    fn os_calls_count_the_bytes_they_move() {
        let (mut metrics, capture) = recorder();
        metrics.begin_turn(&feed());
        metrics.event(&event(pb::child_event::Kind::OsCall(pb::OsCall {
            call_id: 1,
            call: Some(Call::WriteText(pb::os_call::TextWrite {
                path: "/mnt/f.txt".to_owned(),
                data: "hello".to_owned(),
            })),
        })));
        metrics.begin_turn(&resume_call(pb::ext_function_result::Kind::ReturnValue(
            MontyObject::None.into(),
        )));
        metrics.event(&event(pb::child_event::Kind::OsCall(pb::OsCall {
            call_id: 2,
            call: Some(Call::ReadText("/mnt/f.txt".to_owned())),
        })));
        metrics.begin_turn(&resume_call(pb::ext_function_result::Kind::ReturnValue(
            MontyObject::String("hello".to_owned()).into(),
        )));

        assert_eq!(
            capture.values("monty.os.io.bytes"),
            [MetricValue::I64(5), MetricValue::I64(5)]
        );
        assert_eq!(
            capture.attributes("monty.os.io.bytes"),
            [
                [("direction".to_owned(), "write".to_owned())],
                [("direction".to_owned(), "read".to_owned())]
            ]
        );
        let functions: Vec<_> = capture
            .attributes("monty.os.call.duration")
            .iter()
            .map(|attributes| attribute(attributes, "function").unwrap().to_owned())
            .collect();
        assert_eq!(functions, ["write_text", "read_text"]);
    }

    /// The worker reports its execution clock cumulatively, so each run
    /// contributes the delta while the budget ratio uses the running total.
    #[test]
    fn execution_time_is_a_delta_and_the_budget_a_total() {
        let (mut metrics, capture) = recorder();
        for total in [100, 250] {
            metrics.begin_turn(&feed());
            metrics.event(&pb::ChildEvent {
                kind: Some(pb::child_event::Kind::Complete(pb::Complete { value: None })),
                total_execution_micros: total,
                max_duration_micros: Some(1000),
                restored_script_name: None,
            });
        }

        assert_eq!(
            capture.values("monty.run.execution_time"),
            [
                MetricValue::F64(Duration::from_micros(100).as_secs_f64()),
                MetricValue::F64(Duration::from_micros(150).as_secs_f64())
            ]
        );
        assert_eq!(
            capture.values("monty.run.duration_budget_used"),
            [MetricValue::F64(0.1), MetricValue::F64(0.25)]
        );
    }

    /// The sandbox can raise a class of any name, so only the interpreter's own
    /// exception types are recorded; everything else collapses into one series.
    #[test]
    fn exception_types_are_bounded() {
        let (mut metrics, capture) = recorder();
        for exc_type in ["ValueError", "MyCustomError"] {
            metrics.begin_turn(&feed());
            metrics.event(&event(pb::child_event::Kind::Error(pb::Error {
                exception: Some(pb::RaisedException {
                    exc_type: exc_type.to_owned(),
                    message: None,
                    traceback: vec![],
                    data: None,
                }),
            })));
        }

        assert_eq!(
            capture.attributes("monty.errors"),
            [
                [("exc_type".to_owned(), "ValueError".to_owned())],
                [("exc_type".to_owned(), "other".to_owned())]
            ]
        );
        let runs = capture.attributes("monty.run.duration");
        assert_eq!(attribute(&runs[0], "outcome"), Some("error"));
    }

    /// A dump is a housekeeping turn: it reports its own size and duration and
    /// leaves the feed it interrupted open.
    #[test]
    fn a_dump_reports_its_size_without_ending_the_run() {
        let (mut metrics, capture) = recorder();
        metrics.begin_turn(&feed());
        metrics.begin_turn(&request(pb::parent_request::Kind::Dump(pb::Dump {})));
        metrics.event(&event(pb::child_event::Kind::DumpResult(pb::DumpResult {
            state: vec![0; 32],
        })));

        assert_eq!(capture.values("monty.snapshot.bytes"), [MetricValue::I64(32)]);
        let turns = capture.attributes("monty.turn.duration");
        assert_eq!(attribute(&turns[0], "turn"), Some("dump"));
        assert!(capture.values("monty.run.duration").is_empty());
    }

    /// A Rust host records into instruments of its own rather than through an
    /// adapter: the measurements have to reach its meter provider, and the
    /// duration histograms have to come out exponential (the SDK's default
    /// buckets would put every monty turn in the first one).
    #[test]
    fn a_logfire_host_records_into_its_own_meter() {
        let exporter = InMemoryMetricExporter::default();
        let logfire = logfire::configure()
            .local()
            .send_to_logfire(false)
            .with_metrics(Some(
                MetricsOptions::default().with_additional_reader(PeriodicReader::builder(exporter.clone()).build()),
            ))
            .finish()
            .unwrap();
        let mut metrics = TurnMetrics::new(Metrics::for_logfire(logfire.clone()));

        metrics.begin_turn(&feed());
        metrics.event(&call_event("double"));
        metrics.begin_turn(&resume_call(pb::ext_function_result::Kind::ReturnValue(
            MontyObject::Int(4).into(),
        )));
        metrics.event(&event(pb::child_event::Kind::Complete(pb::Complete { value: None })));
        logfire.force_flush().unwrap();

        let exported = exporter.get_finished_metrics().unwrap();
        let mut found = Vec::new();
        for resource in &exported {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics() {
                    found.push((metric.name().to_owned(), is_exponential(metric.data())));
                }
            }
        }
        found.sort();
        assert_eq!(
            found,
            [
                ("monty.host.call.duration".to_owned(), true),
                ("monty.run.duration".to_owned(), true),
                ("monty.run.execution_time".to_owned(), true),
                ("monty.run.suspensions".to_owned(), false),
            ]
        );
    }

    /// Whether an exported metric used base-2 exponential bucketing.
    fn is_exponential(data: &AggregatedMetrics) -> bool {
        match data {
            AggregatedMetrics::F64(data) => matches!(data, MetricData::ExponentialHistogram(_)),
            AggregatedMetrics::U64(data) => matches!(data, MetricData::ExponentialHistogram(_)),
            AggregatedMetrics::I64(data) => matches!(data, MetricData::ExponentialHistogram(_)),
        }
    }
}
