//! Logfire instrumentation of the pool's protocol conversations.
//!
//! The pool builds every `ParentRequest` and decodes every `ChildEvent`
//! anyway, so the sandbox needs no instrumenting. The trace mirrors the
//! protocol: `Configure` opens a session span `Reset` closes, `Feed` opens a
//! span held across suspensions, and each suspension is a child span the
//! answering `Resume*` closes — its duration is the host round-trip.
//!
//! Logfire runs in *local* mode, leaving the host's own tracing/OTel setup
//! alone, so every recording call must scope the pool's logfire in via
//! [`set_local_logfire`] and spans are parented explicitly rather than
//! entered — a checkout's turns can run on different threads, so an
//! entered-span stack would be wrong and make [`crate::Checkout`] `!Send`.

use std::process;

use logfire::{ConfigureError, Logfire, config::AdvancedOptions, set_local_logfire};
use monty_proto::{WireFunctionCall, WireObject, pb, pb::os_call::Call};
use monty_types::{MontyObject, bytes_repr};
use opentelemetry::{KeyValue, Value as OtelValue};
use opentelemetry_sdk::Resource;
use tracing::{Span, level_filters::LevelFilter};
use uuid::Uuid;

use crate::telemetry_json::{
    nonfinite_str, serialize_capped, serialize_dict_capped, serialize_named_capped, serialize_seq_capped,
};

/// Configures a pool-scoped logfire with `token` and records that the pool
/// started.
///
/// Local mode (see the module docs) makes the returned [`Logfire`] the only
/// handle: the pool must keep it alive and `shutdown` it to flush the exporter.
/// Everything here is recorded at INFO, so `RUST_LOG` can filter it out.
pub(crate) fn init(token: String) -> Result<Logfire, ConfigureError> {
    // `builder_empty` so this adds only these two attributes: logfire fills in
    // service name/version and the sdk resource itself. UUIDv7 so instance ids
    // sort by pool start.
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
/// Lives on the [`crate::worker::Worker`], which sees every request and event.
/// A disabled recorder (no `logfire_token`) is a no-op that skips rendering
/// values at all, so the worker is written the same way either way. Spans close
/// by being dropped, so a worker that dies mid-turn still closes its own.
pub(crate) struct Recorder {
    /// `None` disables the recorder entirely.
    logfire: Option<Logfire>,
    /// OS pid of the worker, recorded on its session spans (`None` for a
    /// remote WebSocket worker).
    worker_pid: Option<u32>,
    /// The span of a housekeeping turn (`Load`, `Dump`, ...), closed by the
    /// turn-ending event. Innermost: a `Dump` can run mid-suspension.
    turn: Option<Span>,
    /// The span of the suspension the feed is blocked on, closed when the
    /// answering `Resume*` is sent so its duration is the host round-trip.
    pending: Option<Span>,
    /// The in-flight feed's span, held across suspension round-trips so the
    /// whole feed is one span. Closed by the turn-ending event.
    feed: Option<Span>,
    /// The session span every turn nests inside; `Configure` opens it and
    /// `Reset` closes it.
    session: Option<Span>,
    /// Whether the current turn is a `Dump`: an `Error` reply to one leaves
    /// the feed suspended and resumable, so it closes only the dump span.
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

    /// Starts recording one turn; called once the frame is on the wire, so a
    /// rejected oversize frame records nothing.
    pub(crate) fn begin_turn(&mut self, request: &pb::ParentRequest) {
        let Some(logfire) = &self.logfire else { return };
        let _guard = set_local_logfire(logfire.clone());
        // a turn span whose ending event never arrived (worker died mid-turn,
        // undecodable reply) closes here rather than leaking open
        self.turn = None;
        self.dump_turn = false;
        match &request.kind {
            // a stale session (impossible via the checkout state machine, but
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
                    // i64: `tracing` has no typed u32 value, so a u32 would be
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
            // ends the session: closing the spans is the whole record, since
            // the bare `Ok` it is answered with would say nothing. A reset can
            // land mid-suspension, so close innermost-first.
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
            // the resume family opens no span: it records the host's answer
            // inside the pending suspension span, then closes it
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

    /// Records one event from the worker: suspension events open the pending
    /// span, turn-ending events close the feed and turn spans.
    pub(crate) fn event(&mut self, event: &pb::ChildEvent) {
        let Some(logfire) = &self.logfire else { return };
        let _guard = set_local_logfire(logfire.clone());
        // the budget travels with the elapsed time so `Load`-restored sessions,
        // whose limits come from the dump, show what it is measured against
        let micros = event.total_execution_micros;
        let max_duration = event.max_duration_micros;
        // only a `Load` reply carries this: the session span already exists with
        // the `Configure` name, so the dump's name goes on the load turn
        if let Some(script_name) = &event.restored_script_name {
            logfire::info!(parent: self.context_span(), "restored {script_name}", script_name = script_name);
        }
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
                // an error reply to `Dump` leaves the feed suspended and
                // resumable, so it closes only the dump span
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
            // shutting down); its final state dump is absent when there was no
            // session or the dump failed. The worker is discarded right after,
            // closing its spans.
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
    /// inside it before the caller drops it, closing it.
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

/// Renders a feed's named inputs as one JSON object; the bool reports a cut at
/// [`ATTR_SIZE_LIMIT`]. Values are borrowed, never cloned — inputs can be a
/// large graph of which only the cap survives.
fn render_inputs(inputs: &[pb::NamedValue]) -> (Option<String>, bool) {
    if inputs.is_empty() {
        return (None, false);
    }
    // one placeholder, borrowed by every value the frame left out
    let missing = MontyObject::Repr(MISSING.to_owned());
    let pairs: Vec<(&str, &MontyObject)> = inputs
        .iter()
        .map(|i| {
            let value = i.value.as_ref().and_then(|v| v.0.as_ref()).unwrap_or(&missing);
            (i.name.as_str(), value)
        })
        .collect();
    let (json, cut) = serialize_named_capped(&pairs, ATTR_SIZE_LIMIT);
    (Some(json), cut)
}

/// Renders a call's positional arguments as a JSON list and its keyword
/// arguments as a JSON object, either absent when empty; borrowed for the
/// reason given in [`render_inputs`].
fn render_call_arguments(call: &WireFunctionCall) -> (Option<String>, Option<String>, bool) {
    let mut any_cut = false;
    let args = (!call.args.is_empty()).then(|| {
        let (json, cut) = serialize_seq_capped(&call.args, ATTR_SIZE_LIMIT);
        any_cut |= cut;
        json
    });
    let kwargs = (!call.kwargs.is_empty()).then(|| {
        let (json, cut) = serialize_dict_capped(&call.kwargs, ATTR_SIZE_LIMIT);
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
        // the host writes both of these, so both need the cap
        Some(pb::ext_function_result::Kind::Error(e)) => {
            let (text, cut) = truncate_str(&format!("raise {}", render_exception(e)));
            (text.into(), cut)
        }
        Some(pb::ext_function_result::Kind::Future(id)) => (format!("future {id}").into(), false),
        Some(pb::ext_function_result::Kind::NotFound(name)) => {
            let (text, cut) = truncate_str(&format!("not found: {name}"));
            (text.into(), cut)
        }
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

/// Opens the span for one os call suspension: the function name plus each
/// argument as an `args.*` attribute named after its proto field. Every path
/// is a virtual sandbox path.
///
/// Each call shape gets its own macro invocation because the attribute set is
/// baked into the span's `logfire.json_schema` at compile time — a union-shaped
/// call would surface every unused argument as `null` in the UI.
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
        // no default is recorded as `args.default = null`, matching Python's
        // `os.getenv`; a span attribute cannot carry a dynamically typed
        // value, so the default is stringified
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

/// Records an `Error` event: exception type, message, traceback, and each
/// field of a structured payload as an `exc_data.*` attribute — including the
/// offending input, the value being debugged. One macro invocation per payload
/// shape, for the reason given in [`os_call_span`].
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
            // the codec name is whatever the sandbox passed to `decode`, so it
            // needs the cap as much as the object does
            let (encoding, encoding_cut) = truncate_str(&u.encoding);
            let (reason, reason_cut) = truncate_str(&u.reason);
            error_event!(
                base_cut | object_cut | encoding_cut | reason_cut,
                exc_data.encoding = encoding,
                exc_data.object = object,
                exc_data.start = u.start,
                exc_data.end = u.end,
                exc_data.reason = reason,
            );
        }
        Some(pb::exc_data::Kind::Json(j)) => {
            let (doc, doc_cut) = unzip(j.doc.as_deref().map(truncate_str));
            let (msg, msg_cut) = truncate_str(&j.msg);
            error_event!(
                base_cut | doc_cut | msg_cut,
                exc_data.msg = msg,
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
/// the b'' wrapper, capped at [`ATTR_SIZE_LIMIT`]. Only that many input bytes
/// are escaped — each escapes to at least one character, so the rest would
/// inflate a whole `write_bytes` payload just to throw it away.
fn bytes_attr(b: &[u8]) -> (String, bool) {
    let head = &b[..b.len().min(ATTR_SIZE_LIMIT)];
    let repr = bytes_repr(head);
    let (text, cut) = truncate_str(&repr[2..repr.len() - 1]);
    (text, cut | (head.len() < b.len()))
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
    use opentelemetry::{logs::AnyValue, trace::SpanId};
    use opentelemetry_sdk::{
        logs::{InMemoryLogExporter, SimpleLogProcessor},
        trace::{InMemorySpanExporter, SimpleSpanProcessor, SpanData},
    };

    use super::{ATTR_SIZE_LIMIT, Recorder, bytes_attr, render_ext_result};

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

    /// Drives a whole session and checks the exported spans nest and the log
    /// records land in the right ones — this is what proves the explicit-parent
    /// plumbing (see the module docs) holds up.
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

    /// Every attribute a host or the sandbox can make arbitrarily long is
    /// capped and flagged, including those inside an `ExtFunctionResult`.
    #[test]
    fn oversize_attributes_are_capped_and_flagged() {
        let long = "x".repeat(ATTR_SIZE_LIMIT * 2);
        let result = pb::ExtFunctionResult {
            kind: Some(pb::ext_function_result::Kind::Error(pb::RaisedException {
                exc_type: "ValueError".to_owned(),
                message: Some(long.clone()),
                traceback: vec![],
                data: None,
            })),
        };
        let (value, cut) = render_ext_result(Some(&result));
        assert!(cut);
        assert_eq!(value.as_str().len(), ATTR_SIZE_LIMIT);

        let (name, cut) = render_ext_result(Some(&pb::ExtFunctionResult {
            kind: Some(pb::ext_function_result::Kind::NotFound(long)),
        }));
        assert!(cut);
        assert_eq!(name.as_str().len(), ATTR_SIZE_LIMIT);

        // a payload one byte past the cap: the escaped head fills it exactly,
        // so only the dropped input marks it as cut
        let (text, cut) = bytes_attr(&vec![b'a'; ATTR_SIZE_LIMIT + 1]);
        assert!(cut);
        assert_eq!(text.len(), ATTR_SIZE_LIMIT);
        assert_eq!(bytes_attr(b"hi\xff"), ("hi\\xff".to_owned(), false));
    }

    /// The `Error` event's structured `exc_data.*` payload fields are capped
    /// and flagged too.
    #[test]
    fn oversize_error_payload_is_capped() {
        let (logfire, _spans, logs) = test_logfire();
        let mut recorder = Recorder::new(Some(logfire), None);
        recorder.event(&event(pb::child_event::Kind::Error(pb::Error {
            exception: Some(pb::RaisedException {
                exc_type: "UnicodeDecodeError".to_owned(),
                message: None,
                traceback: vec![],
                data: Some(pb::ExcData {
                    kind: Some(pb::exc_data::Kind::Unicode(pb::UnicodeErrorData {
                        encoding: "u".repeat(ATTR_SIZE_LIMIT * 2),
                        object: None,
                        start: 0,
                        end: 1,
                        reason: "bad".to_owned(),
                    })),
                }),
            }),
        })));

        let logs = logs.get_emitted_logs().unwrap();
        let record = &logs.first().unwrap().record;
        let attr = |key: &str| {
            record
                .attributes_iter()
                .find(|(k, _)| k.as_str() == key)
                .map(|(_, v)| v.clone())
        };
        let Some(AnyValue::String(encoding)) = attr("exc_data.encoding") else {
            panic!("expected a string encoding attribute");
        };
        assert_eq!(encoding.as_str().len(), ATTR_SIZE_LIMIT);
        assert_eq!(attr("length_limit_exceeded"), Some(AnyValue::Boolean(true)));
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
