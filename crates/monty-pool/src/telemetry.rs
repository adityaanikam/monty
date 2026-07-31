//! Logfire instrumentation of the pool's protocol conversations.
//!
//! The pool records every worker's wire traffic from the host side: it builds
//! each `ParentRequest` and decodes each `ChildEvent` anyway, so the whole
//! conversation is observable without instrumenting the sandbox itself. A
//! worker serves one REPL session per checkout, so the trace is shaped the
//! same way: `Configure` opens a session span carrying its arguments and
//! `Reset` closes it. Each `Feed` opens a span held across suspension
//! round-trips, so the whole logical feed is one span: every suspension
//! (external function call, os call, name lookup, future resolution) becomes
//! a child span closed when the pool's `Resume*` answer is sent — its
//! duration is the host round-trip — and the turn-ending
//! `Complete`/`Error`/`TypingError` event closes the feed span. Housekeeping
//! requests (`Load`, `Dump`, ...) are plain turn spans of their own.
//! Everything the protocol carries is recorded verbatim — code, inputs, call
//! arguments, results, exceptions — except the opaque `Load`/`Dump` snapshot
//! blobs, which are recorded as a byte count.
//!
//! Logfire is configured in *local* mode: nothing global is touched, so the
//! host application keeps its own `tracing`/OTel setup and several pools can
//! coexist in one process. The flip side is that every recording call must
//! scope the pool's own logfire in via [`set_local_logfire`], and spans must
//! be parented explicitly (`parent:`) rather than by thread-local entering —
//! a checkout's turns can run on different threads, so an entered-span stack
//! would both be wrong and make [`crate::Checkout`] `!Send`.

use std::process;

use logfire::{ConfigureError, Logfire, config::AdvancedOptions, set_local_logfire};
use monty_proto::{WireFunctionCall, WireObject, pb, pb::os_call::Call};
use monty_types::{MontyObject, bytes_repr};
use opentelemetry::{KeyValue, Value as OtelValue};
use opentelemetry_sdk::Resource;
use tracing::{Span, level_filters::LevelFilter};
use uuid::Uuid;

use crate::telemetry_json::{nonfinite_str, serialize_capped};

/// Configures a pool-scoped logfire with `token` and records that the pool
/// started.
///
/// Local mode (see the module docs) means the returned [`Logfire`] is the
/// only handle on this configuration: the pool must keep it alive and call
/// `shutdown` on teardown to flush the exporter. The pool identifies itself
/// with a fresh `service.instance.id` (UUIDv7, so ids sort by pool start)
/// plus the host `process.pid`. `RUST_LOG` in the host environment filters
/// what is recorded, as in any logfire-instrumented process; the default
/// level is INFO, which everything recorded here is emitted at.
pub(crate) fn init(token: String) -> Result<Logfire, ConfigureError> {
    // `builder_empty` so this adds only these two attributes: logfire fills
    // in service name/version and the sdk resource itself
    let resource = Resource::builder_empty()
        .with_attributes([
            KeyValue::new("service.instance.id", Uuid::now_v7().to_string()),
            KeyValue::new("process.pid", i64::from(process::id())),
        ])
        .build();
    let logfire = logfire::configure()
        .local()
        .with_token(token)
        .with_service_name("monty")
        .with_service_version(env!("CARGO_PKG_VERSION"))
        .with_default_level_filter(LevelFilter::INFO)
        .with_advanced_options(AdvancedOptions::default().with_resource(resource))
        .finish()?;
    let _guard = set_local_logfire(logfire.clone());
    logfire::info!("monty pool started");
    Ok(logfire)
}

/// Records one worker's protocol turns to the pool's logfire.
///
/// Lives on the [`crate::worker::Worker`], which sees every request sent and
/// every event received — the same vantage point the child's own loop would
/// have. A disabled recorder (pool without a `logfire_token`) makes every
/// method a no-op and, crucially, skips rendering values to strings, so the
/// worker is written the same way whether or not telemetry is on.
///
/// Spans are plain [`Span`] handles closed by dropping them (never entered —
/// see the module docs), so a worker that dies mid-turn closes everything
/// still open when it is dropped, and the spans survive the crash — they were
/// never inside the crashed process.
pub(crate) struct Recorder {
    /// `None` disables the recorder entirely.
    logfire: Option<Logfire>,
    /// OS pid of the worker, recorded on its session spans (`None` for a
    /// remote WebSocket worker).
    worker_pid: Option<u32>,
    /// The span of a housekeeping turn (`Load`, `Dump`, ...), closed by the
    /// turn-ending event. Innermost: a `Dump` can run mid-suspension.
    turn: Option<Span>,
    /// The span of the suspension the feed is blocked on (external function
    /// call, os call, name lookup, future resolution). Opened by the
    /// suspension event, closed when the answering `Resume*` is sent, so its
    /// duration is the host round-trip.
    pending: Option<Span>,
    /// The span of the in-flight feed, held across suspension round-trips so
    /// the whole logical feed is one span. Opened by `Feed`, closed by the
    /// turn-ending `Complete`/`Error`/`TypingError` event.
    feed: Option<Span>,
    /// The session span every turn nests inside. `Configure` opens it,
    /// `Reset` closes it, and dropping the recorder closes one left open by
    /// a crash or shutdown.
    session: Option<Span>,
    /// Whether the current turn is a `Dump`: an `Error` reply to one (e.g. an
    /// oversize dump) leaves the feed suspended and resumable, so it must
    /// close only the dump span, not the feed.
    dump_turn: bool,
}

impl Recorder {
    /// A recorder for one worker; `logfire = None` records nothing.
    pub(crate) const fn new(logfire: Option<Logfire>, worker_pid: Option<u32>) -> Self {
        Self {
            logfire,
            worker_pid,
            turn: None,
            pending: None,
            feed: None,
            session: None,
            dump_turn: false,
        }
    }

    /// Starts recording one turn, called after the request frame is written
    /// (a rejected oversize frame never reaches the worker, so it records
    /// nothing): opens the session span on `Configure`, the feed span on
    /// `Feed`, or a turn span for the housekeeping requests. `Resume*`
    /// requests open no span — they record the host's answer inside the
    /// pending suspension span and close it, so a feed reads as one span with
    /// a child span per suspension.
    pub(crate) fn begin_turn(&mut self, request: &pb::ParentRequest) {
        let Some(logfire) = &self.logfire else { return };
        let _guard = set_local_logfire(logfire.clone());
        // a turn span whose ending event never arrived (worker died mid-turn,
        // undecodable reply) closes here rather than leaking open
        self.turn = None;
        self.dump_turn = false;
        match &request.kind {
            // Configure opens the session span rather than a turn span; a
            // stale session (impossible via the checkout state machine, but
            // cheap to be safe against) is closed by the overwrite
            Some(pb::parent_request::Kind::Configure(c)) => {
                self.pending = None;
                self.feed = None;
                let limits = c.limits.as_ref();
                self.session = Some(logfire::span!(
                    "session {script_name}",
                    script_name = &c.script_name,
                    monty_version = &c.monty_version,
                    type_check = c.type_check,
                    type_check_stubs = c.type_check_stubs.as_ref(),
                    assert_message_annotations = c.assert_message_annotations,
                    max_duration_micros = limits.and_then(|l| l.max_duration_micros),
                    max_memory_bytes = limits.and_then(|l| l.max_memory_bytes),
                    gc_interval = limits.and_then(|l| l.gc_interval),
                    max_recursion_depth = limits.and_then(|l| l.max_recursion_depth),
                    // i64: a u32 has no typed `tracing` value and would be
                    // recorded as its debug string
                    worker_pid = self.worker_pid.map(i64::from),
                ));
            }
            Some(pb::parent_request::Kind::Load(l)) => {
                self.turn = Some(logfire::span!(
                    parent: self.context_span(),
                    "load",
                    state_bytes = l.state.len(),
                ));
            }
            // ends the session: closing the spans is the whole record, and
            // the bare `Ok` it is answered with would say nothing. A reset
            // can land mid-suspension in principle, so close innermost-first.
            Some(pb::parent_request::Kind::Reset(_)) => {
                self.pending = None;
                self.feed = None;
                self.session = None;
            }
            Some(pb::parent_request::Kind::InstallDependencies(d)) => {
                self.turn = Some(logfire::span!(
                    parent: self.context_span(),
                    "install dependencies",
                    requirements = render_str_list(&d.requirements),
                ));
            }
            Some(pb::parent_request::Kind::Feed(f)) => {
                let (code, code_cut) = truncate_str(&f.code);
                let (inputs, inputs_cut) = render_inputs(&f.inputs);
                // a feed while one is open is a checkout-level error the
                // worker rejects; close the stale spans so nesting stays sane
                self.pending = None;
                self.feed = None;
                self.feed = Some(logfire::span!(
                    parent: self.context_span(),
                    "feed",
                    code = &code,
                    code.language = "python",
                    inputs = inputs,
                    skip_type_check = f.skip_type_check,
                    length_limit_exceeded = (code_cut | inputs_cut).then_some(true),
                ));
            }
            // the resume family: record the host's answer inside the pending
            // suspension span, then close it — its duration is the host time
            Some(pb::parent_request::Kind::ResumeCall(r)) => {
                let pending = self.take_pending();
                let (result, cut) = render_ext_result(r.result.as_ref());
                logfire::info!(
                    parent: pending,
                    "call result",
                    call_id = r.call_id,
                    result = result,
                    length_limit_exceeded = cut.then_some(true),
                );
            }
            Some(pb::parent_request::Kind::ResumeNameLookup(r)) => {
                let pending = self.take_pending();
                let (result, cut) = render_name_lookup(r.kind.as_ref());
                logfire::info!(
                    parent: pending,
                    "name lookup result",
                    result = result,
                    length_limit_exceeded = cut.then_some(true),
                );
            }
            Some(pb::parent_request::Kind::ResumeFutures(r)) => {
                let pending = self.take_pending();
                let (results, cut) = render_future_results(&r.results);
                logfire::info!(
                    parent: pending,
                    "future results",
                    results = results,
                    length_limit_exceeded = cut.then_some(true),
                );
            }
            Some(pb::parent_request::Kind::Dump(_)) => {
                self.dump_turn = true;
                self.turn = Some(logfire::span!(parent: self.context_span(), "dump"));
            }
            // no span of its own: the session is normally already reset, and
            // a lone "shutdown" span would start a whole trace for a worker
            // exiting; closing whatever is open is the record
            Some(pb::parent_request::Kind::Shutdown(_)) => {
                self.pending = None;
                self.feed = None;
                self.session = None;
            }
            // `checkout::request` always sets a kind
            None => {}
        }
    }

    /// Records one event received from the worker.
    ///
    /// The suspension events open the pending span the matching `Resume*`
    /// closes; the turn-ending events close the feed and/or housekeeping turn
    /// spans. `DumpResult` records only its snapshot size, for the reason
    /// given in the module docs.
    pub(crate) fn event(&mut self, event: &pb::ChildEvent) {
        let Some(logfire) = &self.logfire else { return };
        let _guard = set_local_logfire(logfire.clone());
        // carried by every turn-ending event; recording the budget alongside
        // keeps `Load`-restored sessions (whose limits come from the dump, not
        // the session span's Configure) showing what the time is measured against
        let micros = event.total_execution_micros;
        let max_duration = event.max_duration_micros;
        match &event.kind {
            Some(pb::child_event::Kind::Print(p)) => {
                let (text, cut) = truncate_str(&p.text);
                logfire::info!(
                    parent: self.context_span(),
                    "print {stream}",
                    stream = print_stream(p.stream),
                    text = text,
                    length_limit_exceeded = cut.then_some(true),
                );
            }
            Some(pb::child_event::Kind::FunctionCall(c)) => {
                let (args, kwargs, cut) = render_call_arguments(c);
                self.pending = Some(logfire::span!(
                    parent: self.context_span(),
                    "function call {function_name}",
                    function_name = &c.function_name,
                    args = args,
                    kwargs = kwargs,
                    call_id = c.call_id,
                    method_call = c.method_call,
                    length_limit_exceeded = cut.then_some(true),
                    total_execution_micros = micros,
                    max_duration_micros = max_duration,
                ));
            }
            Some(pb::child_event::Kind::OsCall(c)) => {
                self.pending = Some(os_call_span(c, micros, max_duration, &self.context_span()));
            }
            Some(pb::child_event::Kind::NameLookup(n)) => {
                self.pending = Some(logfire::span!(
                    parent: self.context_span(),
                    "name lookup {name}",
                    name = &n.name,
                    total_execution_micros = micros,
                    max_duration_micros = max_duration,
                ));
            }
            Some(pb::child_event::Kind::ResolveFutures(r)) => {
                self.pending = Some(logfire::span!(
                    parent: self.context_span(),
                    "resolve futures",
                    pending_call_ids = render_call_ids(&r.pending_call_ids),
                    total_execution_micros = micros,
                    max_duration_micros = max_duration,
                ));
            }
            Some(pb::child_event::Kind::Complete(c)) => {
                let (value, cut) = optional_attr(c.value.as_ref());
                logfire::info!(
                    parent: self.context_span(),
                    "complete",
                    value = value,
                    length_limit_exceeded = cut.then_some(true),
                    total_execution_micros = micros,
                    max_duration_micros = max_duration,
                );
                self.end_feed();
            }
            Some(pb::child_event::Kind::Error(e)) => {
                record_error(e, micros, max_duration, &self.context_span());
                // an error reply to `Dump` (e.g. an oversize dump) does not
                // end the in-flight feed — the worker stays suspended and
                // resumable — so it closes only the dump span
                if self.dump_turn {
                    self.turn = None;
                } else {
                    self.end_feed();
                }
            }
            Some(pb::child_event::Kind::TypingError(t)) => {
                let (diagnostics, cut) = truncate_str(&t.diagnostics);
                logfire::error!(
                    parent: self.context_span(),
                    "typing error",
                    diagnostics = diagnostics,
                    length_limit_exceeded = cut.then_some(true),
                    total_execution_micros = micros,
                    max_duration_micros = max_duration,
                );
                self.end_feed();
            }
            Some(pb::child_event::Kind::DumpResult(d)) => {
                logfire::info!(
                    parent: self.context_span(),
                    "dump result",
                    state_bytes = d.state.len(),
                    total_execution_micros = micros,
                    max_duration_micros = max_duration,
                );
                self.turn = None;
            }
            // only a serving relay sends this (a WebSocket worker's server is
            // shutting down); its final state dump is an opaque blob,
            // recorded by size, absent when there was no session or the dump
            // failed. The worker is discarded right after, closing its spans.
            Some(pb::child_event::Kind::Shutdown(s)) => {
                logfire::info!(
                    parent: self.context_span(),
                    "shutdown",
                    state_bytes = s.dump.as_ref().map(Vec::len),
                    total_execution_micros = micros,
                    max_duration_micros = max_duration,
                );
            }
            // a bare acknowledgement ending a housekeeping turn; the turn
            // span itself is the record
            Some(pb::child_event::Kind::Ok(_)) => self.turn = None,
            Some(pb::child_event::Kind::FatalError(f)) => {
                logfire::error!(parent: self.context_span(), "fatal error", message = &f.message);
            }
            None => logfire::error!(parent: self.context_span(), "event with no kind"),
        }
    }

    /// The innermost open span, used as the explicit parent for new spans and
    /// records; [`Span::none`] (a root record) when nothing is open.
    fn context_span(&self) -> Span {
        self.turn
            .as_ref()
            .or(self.pending.as_ref())
            .or(self.feed.as_ref())
            .or(self.session.as_ref())
            .cloned()
            .unwrap_or_else(Span::none)
    }

    /// Takes the pending suspension span so the resume answer can be recorded
    /// inside it before it closes (by dropping) at the end of the caller.
    fn take_pending(&mut self) -> Span {
        self.pending.take().unwrap_or_else(Span::none)
    }

    /// Closes the feed-scoped spans after a turn-ending
    /// `Complete`/`Error`/`TypingError` event; innermost-first, and a no-op
    /// for the housekeeping turns those events can also end.
    fn end_feed(&mut self) {
        self.turn = None;
        self.pending = None;
        self.feed = None;
    }
}

/// Placeholder for a field the protocol requires but the frame omitted. The
/// worker rejects such frames; telemetry still has to render something.
const MISSING: &str = "<missing>";

/// Renders a feed's named inputs as one JSON object of name → encoded value;
/// the bool reports a cut at [`ATTR_SIZE_LIMIT`]. The inputs are cloned into
/// a dict to encode them; only run when telemetry is enabled.
fn render_inputs(inputs: &[pb::NamedValue]) -> (Option<String>, bool) {
    if inputs.is_empty() {
        return (None, false);
    }
    let pairs = inputs
        .iter()
        .map(|i| {
            let value = i.value.as_ref().and_then(|v| v.0.clone());
            let value = value.unwrap_or_else(|| MontyObject::Repr(MISSING.to_owned()));
            (MontyObject::String(i.name.clone()), value)
        })
        .collect::<Vec<_>>();
    let (json, cut) = serialize_capped(&MontyObject::dict(pairs), ATTR_SIZE_LIMIT);
    (Some(json), cut)
}

/// Renders a suspended call's positional arguments as a JSON list and its
/// keyword arguments as a JSON object; either is absent when empty. The
/// arguments are cloned to encode them; only run when telemetry is enabled.
fn render_call_arguments(call: &WireFunctionCall) -> (Option<String>, Option<String>, bool) {
    let mut any_cut = false;
    let args = (!call.args.is_empty()).then(|| {
        let (json, cut) = serialize_capped(&MontyObject::List(call.args.clone()), ATTR_SIZE_LIMIT);
        any_cut |= cut;
        json
    });
    let kwargs = (!call.kwargs.is_empty()).then(|| {
        let (json, cut) = serialize_capped(&MontyObject::dict(call.kwargs.clone()), ATTR_SIZE_LIMIT);
        any_cut |= cut;
        json
    });
    (args, kwargs, any_cut)
}

/// Renders the pool's answer to a `FunctionCall` / `OsCall` suspension: a
/// returned value in its typed [`attr_value`] form, every other outcome
/// descriptively. The bool reports a cut at [`ATTR_SIZE_LIMIT`].
fn render_ext_result(result: Option<&pb::ExtFunctionResult>) -> (OtelValue, bool) {
    match result.and_then(|r| r.kind.as_ref()) {
        Some(pb::ext_function_result::Kind::ReturnValue(v)) => attr_value(v),
        Some(pb::ext_function_result::Kind::Error(e)) => (format!("raise {}", render_exception(e)).into(), false),
        Some(pb::ext_function_result::Kind::Future(id)) => (format!("future {id}").into(), false),
        Some(pb::ext_function_result::Kind::NotFound(name)) => (format!("not found: {name}").into(), false),
        Some(pb::ext_function_result::Kind::NotHandled(_)) => ("not handled".into(), false),
        None => (MISSING.into(), false),
    }
}

/// Renders the pool's answer to a `NameLookup` suspension; the bool reports
/// a cut at [`ATTR_SIZE_LIMIT`].
fn render_name_lookup(kind: Option<&pb::resume_name_lookup::Kind>) -> (OtelValue, bool) {
    match kind {
        Some(pb::resume_name_lookup::Kind::Value(v)) => attr_value(v),
        Some(pb::resume_name_lookup::Kind::Undefined(_)) => ("undefined".into(), false),
        None => (MISSING.into(), false),
    }
}

/// Renders resolved futures as `call_id: result, ...`; the bool reports a cut
/// of any result — or of the joined output — at [`ATTR_SIZE_LIMIT`].
fn render_future_results(results: &[pb::FutureResult]) -> (Option<String>, bool) {
    if results.is_empty() {
        return (None, false);
    }
    let mut any_cut = false;
    let entries: Vec<String> = results
        .iter()
        .map(|r| {
            let (result, cut) = render_ext_result(r.result.as_ref());
            any_cut |= cut;
            format!("{}: {result}", r.call_id)
        })
        .collect();
    let (joined, cut) = truncate_str(&entries.join(", "));
    (Some(joined), any_cut | cut)
}

/// Renders the ids a `ResolveFutures` suspension is blocked on as a JSON list.
fn render_call_ids(ids: &[u32]) -> Option<String> {
    (!ids.is_empty()).then(|| serde_json::to_string(ids).unwrap_or_default())
}

/// Opens the span for one os call suspension: the function name plus each of
/// its arguments as a standalone `args.*` attribute named after the proto
/// field (strings as strings, numbers as numbers, bools as bools; `bytes`
/// data as its Python repr). Every path is a virtual sandbox path. The span
/// stays open until the pool's answer closes it.
///
/// Each call shape gets its own macro invocation because the attribute set is
/// baked into the span's `logfire.json_schema` at compile time — a single
/// union-shaped call would surface every unused argument as `null` in the UI.
fn os_call_span(os_call: &pb::OsCall, micros: u64, max_duration: Option<u64>, parent: &Span) -> Span {
    let call_id = os_call.call_id;
    /// One span with only the given `args.*` attributes plus the shared tail.
    macro_rules! os_call {
        ($function:expr $(, $($key:ident).+ = $value:expr)* $(,)?) => {
            logfire::span!(
                parent: parent,
                "os call {function}",
                function = $function,
                $($($key).+ = $value,)*
                call_id = call_id,
                total_execution_micros = micros,
                max_duration_micros = max_duration,
            )
        };
    }
    match os_call.call.as_ref() {
        Some(Call::Exists(p)) => os_call!("exists", args.path = p),
        Some(Call::IsFile(p)) => os_call!("is_file", args.path = p),
        Some(Call::IsDir(p)) => os_call!("is_dir", args.path = p),
        Some(Call::IsSymlink(p)) => os_call!("is_symlink", args.path = p),
        Some(Call::ReadText(p)) => os_call!("read_text", args.path = p),
        Some(Call::ReadBytes(p)) => os_call!("read_bytes", args.path = p),
        Some(Call::Stat(p)) => os_call!("stat", args.path = p),
        Some(Call::Iterdir(p)) => os_call!("iterdir", args.path = p),
        Some(Call::Resolve(p)) => os_call!("resolve", args.path = p),
        Some(Call::Absolute(p)) => os_call!("absolute", args.path = p),
        Some(Call::Unlink(p)) => os_call!("unlink", args.path = p),
        Some(Call::Rmdir(p)) => os_call!("rmdir", args.path = p),
        Some(Call::WriteText(w)) => {
            let (data, cut) = truncate_str(&w.data);
            os_call!(
                "write_text",
                args.path = &w.path,
                args.data = data,
                length_limit_exceeded = cut.then_some(true)
            )
        }
        Some(Call::AppendText(w)) => {
            let (data, cut) = truncate_str(&w.data);
            os_call!(
                "append_text",
                args.path = &w.path,
                args.data = data,
                length_limit_exceeded = cut.then_some(true)
            )
        }
        Some(Call::WriteBytes(w)) => {
            let (data, cut) = bytes_attr(&w.data);
            os_call!(
                "write_bytes",
                args.path = &w.path,
                args.data = data,
                length_limit_exceeded = cut.then_some(true)
            )
        }
        Some(Call::AppendBytes(w)) => {
            let (data, cut) = bytes_attr(&w.data);
            os_call!(
                "append_bytes",
                args.path = &w.path,
                args.data = data,
                length_limit_exceeded = cut.then_some(true)
            )
        }
        Some(Call::Open(o)) => os_call!("open", args.path = &o.path, args.mode = &o.mode),
        Some(Call::Mkdir(m)) => os_call!(
            "mkdir",
            args.path = &m.path,
            args.parents = m.parents,
            args.exist_ok = m.exist_ok
        ),
        Some(Call::Rename(r)) => os_call!("rename", args.src = &r.src, args.dst = &r.dst),
        // a getenv with no default is recorded with `args.default = null`,
        // matching the `None` that `os.getenv` defaults to in Python; span
        // attributes cannot carry a dynamic typed value, so it is stringified
        Some(Call::Getenv(g)) => {
            let (default, cut) = optional_attr(g.default.as_ref());
            let default = default.map(|v| v.as_str().into_owned());
            os_call!(
                "getenv",
                args.key = &g.key,
                args.default = default,
                length_limit_exceeded = cut.then_some(true)
            )
        }
        Some(Call::GetEnviron(_)) => os_call!("get_environ"),
        Some(Call::DateToday(_)) => os_call!("date_today"),
        // a null tz name marks a fixed-offset timezone; a naive call has no args
        Some(Call::DateTimeNow(n)) => {
            if let Some(tz) = &n.tz {
                os_call!(
                    "date_time_now",
                    args.tz_offset_seconds = tz.offset_seconds,
                    args.tz_name = tz.name.as_ref()
                )
            } else {
                os_call!("date_time_now")
            }
        }
        None => os_call!(MISSING),
    }
}

/// Renders an exception as `Type: message`.
fn render_exception(exc: &pb::RaisedException) -> String {
    match &exc.message {
        Some(message) => format!("{}: {message}", exc.exc_type),
        None => exc.exc_type.clone(),
    }
}

/// Renders a traceback as one newline-separated string of `file:line in
/// frame` entries, outermost first; the bool reports a cut at
/// [`ATTR_SIZE_LIMIT`].
fn render_traceback(frames: &[pb::StackFrame]) -> (Option<String>, bool) {
    if frames.is_empty() {
        return (None, false);
    }
    let entries: Vec<String> = frames
        .iter()
        .map(|f| {
            let line = f.start.as_ref().map_or(0, |loc| loc.line);
            let name = f.frame_name.as_deref().unwrap_or("<module>");
            format!("{}:{line} in {name}", f.filename)
        })
        .collect();
    let (traceback, cut) = truncate_str(&entries.join("\n"));
    (Some(traceback), cut)
}

/// Records an `Error` event: exception type, message, traceback, and — for
/// the exception types carrying a structured payload — each payload field as
/// a standalone `exc_data.*` attribute, including the offending input (it is
/// the value being debugged).
///
/// Like [`os_call_span`], each payload shape gets its own macro invocation
/// so records don't surface the other shape's attributes as `null`.
fn record_error(error: &pb::Error, micros: u64, max_duration: Option<u64>, parent: &Span) {
    let exc = error.exception.as_ref();
    let exc_type = exc.map_or_else(|| MISSING.to_owned(), |e| e.exc_type.clone());
    let (message, message_cut) = unzip(exc.and_then(|e| e.message.as_deref()).map(truncate_str));
    let (traceback, traceback_cut) = render_traceback(exc.map_or(&[], |e| &e.traceback));
    let base_cut = message_cut | traceback_cut;
    /// One error record with the given cut flag and `exc_data.*` attributes
    /// plus the shared fields.
    macro_rules! error_event {
        ($cut:expr $(, $($key:ident).+ = $value:expr)* $(,)?) => {
            logfire::error!(
                // reborrowed because the macro takes the parent by reference
                parent: *parent,
                "error {exc_type}",
                exc_type = &exc_type,
                exc_message = message,
                traceback = traceback,
                $($($key).+ = $value,)*
                length_limit_exceeded = ($cut).then_some(true),
                total_execution_micros = micros,
                max_duration_micros = max_duration,
            )
        };
    }
    match exc.and_then(|e| e.data.as_ref()).and_then(|d| d.kind.as_ref()) {
        Some(pb::exc_data::Kind::Unicode(u)) => {
            let (object, object_cut) = match &u.object {
                Some(pb::unicode_error_data::Object::ObjectBytes(b)) => bytes_attr(b),
                Some(pb::unicode_error_data::Object::ObjectStr(s)) => truncate_str(s),
                None => (MISSING.to_owned(), false),
            };
            error_event!(
                base_cut | object_cut,
                exc_data.encoding = &u.encoding,
                exc_data.object = object,
                exc_data.start = u.start,
                exc_data.end = u.end,
                exc_data.reason = &u.reason,
            );
        }
        Some(pb::exc_data::Kind::Json(j)) => {
            let (doc, doc_cut) = unzip(j.doc.as_deref().map(truncate_str));
            error_event!(
                base_cut | doc_cut,
                exc_data.msg = &j.msg,
                exc_data.doc = doc,
                exc_data.pos = j.pos,
                exc_data.lineno = j.lineno,
                exc_data.colno = j.colno,
            );
        }
        None => error_event!(base_cut),
    }
}

/// Byte cap for one value attribute; bigger values are cut off, and the
/// record gets a `length_limit_exceeded` attribute saying so.
const ATTR_SIZE_LIMIT: usize = 64 * 1024;

/// Renders a wire value as a typed OTel attribute: scalars keep their native
/// attribute type, everything else becomes logfire-style JSON text. The bool
/// is true when [`ATTR_SIZE_LIMIT`] cut the output short.
fn attr_value(value: &WireObject) -> (OtelValue, bool) {
    match value.0.as_ref() {
        Some(MontyObject::Bool(b)) => ((*b).into(), false),
        Some(MontyObject::Int(i)) => ((*i).into(), false),
        Some(MontyObject::Float(f)) if f.is_finite() => ((*f).into(), false),
        // Python `str()` of the non-finite floats JSON cannot carry
        Some(MontyObject::Float(f)) => (nonfinite_str(*f).into(), false),
        Some(MontyObject::String(s)) => {
            let (text, cut) = truncate_str(s);
            (text.into(), cut)
        }
        Some(MontyObject::Bytes(b)) => {
            let (text, cut) = bytes_attr(b);
            (text.into(), cut)
        }
        Some(other) => {
            let (json, cut) = serialize_capped(other, ATTR_SIZE_LIMIT);
            (json.into(), cut)
        }
        None => (MISSING.into(), false),
    }
}

/// [`attr_value`] for an optional field: an absent value renders as an absent
/// attribute, not as [`MISSING`].
fn optional_attr(value: Option<&WireObject>) -> (Option<OtelValue>, bool) {
    unzip(value.map(attr_value))
}

/// Splits an optional `(value, cut)` pair into the optional value and the cut
/// flag, so an absent attribute contributes no `length_limit_exceeded`.
fn unzip<T>(pair: Option<(T, bool)>) -> (Option<T>, bool) {
    pair.map_or((None, false), |(value, cut)| (Some(value), cut))
}

/// A bytes value as logfire renders bytes: the repr's escaped content without
/// the b'' wrapper, as raw text capped at [`ATTR_SIZE_LIMIT`].
fn bytes_attr(b: &[u8]) -> (String, bool) {
    let repr = bytes_repr(b);
    truncate_str(&repr[2..repr.len() - 1])
}

/// Renders strings as a JSON list, absent when empty.
fn render_str_list(items: &[String]) -> Option<String> {
    (!items.is_empty()).then(|| serde_json::to_string(items).unwrap_or_default())
}

/// Truncates a raw string attribute to [`ATTR_SIZE_LIMIT`] bytes on a char
/// boundary; the bool reports whether anything was cut off.
fn truncate_str(s: &str) -> (String, bool) {
    if s.len() <= ATTR_SIZE_LIMIT {
        (s.to_owned(), false)
    } else {
        let mut end = ATTR_SIZE_LIMIT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        (s[..end].to_owned(), true)
    }
}

/// The human name of a `PrintStream` enum value, for the print event's message.
fn print_stream(stream: i32) -> &'static str {
    match pb::PrintStream::try_from(stream) {
        Ok(pb::PrintStream::Stdout) => "stdout",
        Ok(pb::PrintStream::Stderr) => "stderr",
        _ => "unspecified",
    }
}

// tests live here rather than in `tests/` because `Recorder` is crate-private:
// recording is a side effect of the worker, not part of the pool's public API
#[cfg(test)]
mod tests {
    use logfire::{Logfire, config::AdvancedOptions};
    use monty_proto::{WireFunctionCall, pb};
    use monty_types::MontyObject;
    use opentelemetry::trace::SpanId;
    use opentelemetry_sdk::{
        logs::{InMemoryLogExporter, SimpleLogProcessor},
        trace::{InMemorySpanExporter, SimpleSpanProcessor, SpanData},
    };

    use super::Recorder;

    /// A local logfire capturing spans and logs in memory instead of exporting.
    fn test_logfire() -> (Logfire, InMemorySpanExporter, InMemoryLogExporter) {
        let spans = InMemorySpanExporter::default();
        let logs = InMemoryLogExporter::default();
        let logfire = logfire::configure()
            .local()
            .send_to_logfire(false)
            .with_additional_span_processor(SimpleSpanProcessor::new(spans.clone()))
            .with_advanced_options(AdvancedOptions::default().with_log_processor(SimpleLogProcessor::new(logs.clone())))
            .finish()
            .unwrap();
        (logfire, spans, logs)
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
            total_execution_micros: 42,
            max_duration_micros: None,
            restored_script_name: None,
        }
    }

    /// Drives one whole session — configure, feed, a suspension round-trip,
    /// completion, reset — and checks the exported span tree: the suspension
    /// span is a child of the feed span, the feed span of the session span,
    /// the session span a root; and the resume/complete log records land
    /// inside the right spans. This is what proves the explicit-parent
    /// plumbing (spans are never entered — see the module docs) holds up.
    #[test]
    fn one_feed_produces_a_nested_span_tree() {
        let (logfire, spans, logs) = test_logfire();
        let mut recorder = Recorder::new(Some(logfire), Some(4321));

        recorder.begin_turn(&request(pb::parent_request::Kind::Configure(pb::Configure {
            script_name: "main.py".to_owned(),
            limits: None,
            type_check: false,
            type_check_stubs: None,
            monty_version: "0.0.1".to_owned(),
            assert_message_annotations: None,
        })));
        recorder.begin_turn(&request(pb::parent_request::Kind::Feed(pb::Feed {
            code: "double(2)".to_owned(),
            inputs: vec![],
            skip_type_check: false,
        })));
        recorder.event(&event(pb::child_event::Kind::FunctionCall(WireFunctionCall {
            function_name: "double".to_owned(),
            args: vec![MontyObject::Int(2)],
            kwargs: vec![],
            call_id: 1,
            method_call: false,
        })));
        recorder.begin_turn(&request(pb::parent_request::Kind::ResumeCall(pb::ResumeCall {
            call_id: 1,
            result: Some(pb::ExtFunctionResult {
                kind: Some(pb::ext_function_result::Kind::ReturnValue(MontyObject::Int(4).into())),
            }),
        })));
        recorder.event(&event(pb::child_event::Kind::Complete(pb::Complete {
            value: Some(MontyObject::Int(4).into()),
        })));
        recorder.begin_turn(&request(pb::parent_request::Kind::Reset(pb::Reset {})));

        // spans are exported innermost-first as they close
        let spans = spans.get_finished_spans().unwrap();
        let names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();
        assert_eq!(
            names,
            ["function call {function_name}", "feed", "session {script_name}"]
        );
        let by_name = |name: &str| -> &SpanData { spans.iter().find(|s| s.name == name).unwrap() };
        let session = by_name("session {script_name}");
        let feed = by_name("feed");
        let call = by_name("function call {function_name}");
        assert_eq!(session.parent_span_id, SpanId::INVALID);
        assert_eq!(feed.parent_span_id, session.span_context.span_id());
        assert_eq!(call.parent_span_id, feed.span_context.span_id());
        // the worker's identity is a session attribute (the child-side
        // recorder used to carry it as a per-process resource attribute)
        let worker_pid = session.attributes.iter().find(|kv| kv.key.as_str() == "worker_pid");
        assert_eq!(worker_pid.unwrap().value, 4321.into());

        // the resume answer lands in the suspension span, completion in the feed
        let logs = logs.get_emitted_logs().unwrap();
        let parent_of = |name: &str| {
            logs.iter()
                .find(|l| l.record.event_name() == Some(name))
                .unwrap()
                .record
                .trace_context()
                .unwrap()
                .span_id
        };
        assert_eq!(parent_of("call result"), call.span_context.span_id());
        assert_eq!(parent_of("complete"), feed.span_context.span_id());
    }

    /// A disabled recorder (pool without a token) records nothing and holds
    /// no spans, whatever passes through it.
    #[test]
    fn disabled_recorder_is_inert() {
        let mut recorder = Recorder::new(None, None);
        recorder.begin_turn(&request(pb::parent_request::Kind::Feed(pb::Feed {
            code: "1".to_owned(),
            inputs: vec![],
            skip_type_check: false,
        })));
        recorder.event(&event(pb::child_event::Kind::Complete(pb::Complete { value: None })));
        assert!(recorder.session.is_none() && recorder.feed.is_none() && recorder.pending.is_none());
    }
}
