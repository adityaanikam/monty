//! The WebAssembly Component Model Monty worker.
//!
//! This is the browser analog of `monty subprocess`: a persistent [`Child`]
//! consumes semantic WIT requests and returns semantic events. The child still
//! shares `monty-proto`'s state machine, but protobuf bytes never cross the
//! component boundary or enter the TypeScript host.

use std::{cell::RefCell, io};

use monty_proto::{
    DEFAULT_MAX_DECODE_BYTES, FrameError, MAX_FRAME_LEN, MONTY_VERSION, exceeds_max_frame_len, pb,
    worker::{Child, EventSink, HandleOutcome, protocol_violation},
};
use monty_types::{ExcType, MontyException, MontyObject, OsFunctionCall};

#[expect(
    clippy::same_length_and_capacity,
    reason = "generated canonical-ABI list lifting uses Vec::from_raw_parts"
)]
mod bindings {
    wit_bindgen::generate!({ world: "monty-runtime" });
}

mod value;

use bindings::exports::pydantic::monty::worker::{
    CallResult, ConfigureRequest, DispatchResult, Event, FunctionCallEvent, Guest, NameLookupResult, OsCallEvent,
    PrintEvent, RaisedException, Request, StackFrame, Status, ValuePair,
};

thread_local! {
    /// The session worker, retained for the lifetime of this component instance.
    static CHILD: RefCell<Child> = RefCell::new(Child::new());
}

/// Implements the typed component export over Monty's protocol child.
struct Component;

impl Guest for Component {
    fn dispatch(request: Request) -> DispatchResult {
        CHILD.with_borrow_mut(|child| dispatch(child, request))
    }
}

/// Handles one semantic request while preserving the protocol child's limits.
fn dispatch(child: &mut Child, request: Request) -> DispatchResult {
    let request = match request_from_component(request) {
        Ok(request) => request,
        Err(error) => {
            return DispatchResult {
                status: Status::Continue,
                events: vec![event_from_proto(protocol_violation(&format!(
                    "malformed component request: {error}"
                )))],
            };
        }
    };
    if let Some(len) = exceeds_max_frame_len(&request) {
        return DispatchResult {
            status: Status::Shutdown,
            events: vec![event_from_proto(child.fatal_event(&format!(
                "request frame of {len} bytes exceeds maximum of {MAX_FRAME_LEN} bytes"
            )))],
        };
    }

    let mut sink = ComponentEventSink::default();
    let outcome = match child.handle(request, &mut sink) {
        Ok(outcome) => outcome,
        Err(FrameError::FrameTooLarge { len, max }) => {
            let _ =
                sink.send(&child.fatal_event(&format!("response frame of {len} bytes exceeds maximum of {max} bytes")));
            HandleOutcome::Shutdown
        }
        Err(error) => {
            let _ = sink.send(&child.fatal_event(&format!("component event sink failed: {error}")));
            HandleOutcome::Shutdown
        }
    };
    DispatchResult {
        status: if matches!(outcome, HandleOutcome::Continue) {
            Status::Continue
        } else {
            Status::Shutdown
        },
        events: sink.events,
    }
}

/// Collects semantic component events while enforcing the protobuf frame cap.
#[derive(Default)]
struct ComponentEventSink {
    events: Vec<Event>,
}

impl EventSink for ComponentEventSink {
    fn send(&mut self, event: &pb::ChildEvent) -> Result<(), FrameError> {
        if let Some(len) = exceeds_max_frame_len(event) {
            Err(FrameError::FrameTooLarge {
                len,
                max: MAX_FRAME_LEN,
            })
        } else if event_values_host_size(event) > DEFAULT_MAX_DECODE_BYTES {
            Err(FrameError::Io(io::Error::other(
                "component event values exceed the host-memory budget",
            )))
        } else {
            self.events.push(event_from_proto(event.clone()));
            Ok(())
        }
    }
}

/// Estimates the expanded host size of values before lifting them into JS.
fn event_values_host_size(event: &pb::ChildEvent) -> usize {
    match &event.kind {
        Some(pb::child_event::Kind::Complete(complete)) => complete
            .value
            .as_ref()
            .and_then(|value| value.0.as_ref())
            .map_or(0, object_host_size),
        Some(pb::child_event::Kind::FunctionCall(call)) => call
            .args
            .iter()
            .chain(call.kwargs.iter().flat_map(|(key, value)| [key, value]))
            .fold(0, |size, value| size.saturating_add(object_host_size(value))),
        Some(pb::child_event::Kind::OsCall(call)) => match &call.call {
            Some(pb::os_call::Call::Getenv(getenv)) => getenv
                .default
                .as_ref()
                .and_then(|value| value.0.as_ref())
                .map_or(0, object_host_size),
            _ => 0,
        },
        _ => 0,
    }
}

/// Estimates one value tree using the same accounting as protobuf decoding.
fn object_host_size(object: &MontyObject) -> usize {
    let mut size = object.host_size();
    match object {
        MontyObject::List(items)
        | MontyObject::Tuple(items)
        | MontyObject::Set(items)
        | MontyObject::FrozenSet(items)
        | MontyObject::NamedTuple { values: items, .. } => {
            for item in items {
                size = size.saturating_add(object_host_size(item));
            }
        }
        MontyObject::Dict(pairs) | MontyObject::Dataclass { attrs: pairs, .. } => {
            for (key, value) in pairs {
                size = size
                    .saturating_add(object_host_size(key))
                    .saturating_add(object_host_size(value));
            }
        }
        _ => {}
    }
    size
}

/// Converts a semantic component request into the child state machine's type.
fn request_from_component(request: Request) -> Result<pb::ParentRequest, String> {
    let kind = match request {
        Request::Configure(request) => pb::parent_request::Kind::Configure(configure_from_component(request)),
        Request::Feed(request) => pb::parent_request::Kind::Feed(pb::Feed {
            code: request.code,
            inputs: request
                .inputs
                .into_iter()
                .map(|input| {
                    Ok(pb::NamedValue {
                        name: input.name,
                        value: Some(value::from_component(input.value)?.into()),
                    })
                })
                .collect::<Result<_, String>>()?,
            skip_type_check: request.skip_type_check,
        }),
        Request::ResumeCall(request) => pb::parent_request::Kind::ResumeCall(pb::ResumeCall {
            call_id: request.call_id,
            result: Some(call_result_from_component(request.outcome)?),
        }),
        Request::ResumeNameLookup(result) => {
            let kind = match result {
                NameLookupResult::Value(value) => {
                    pb::resume_name_lookup::Kind::Value(value::from_component(value)?.into())
                }
                NameLookupResult::Undefined => pb::resume_name_lookup::Kind::Undefined(pb::Unit {}),
            };
            pb::parent_request::Kind::ResumeNameLookup(pb::ResumeNameLookup { kind: Some(kind) })
        }
        Request::ResumeFutures(results) => pb::parent_request::Kind::ResumeFutures(pb::ResumeFutures {
            results: results
                .into_iter()
                .map(|result| {
                    Ok(pb::FutureResult {
                        call_id: result.call_id,
                        result: Some(call_result_from_component(result.outcome)?),
                    })
                })
                .collect::<Result<_, String>>()?,
        }),
        Request::Dump => pb::parent_request::Kind::Dump(pb::Dump {}),
        Request::Load(state) => pb::parent_request::Kind::Load(pb::Load { state }),
        Request::Reset => pb::parent_request::Kind::Reset(pb::Reset {}),
    };
    Ok(pb::ParentRequest {
        kind: Some(kind),
        trace_parent: None,
    })
}

/// Converts session options into the protocol child's configuration type.
fn configure_from_component(request: ConfigureRequest) -> pb::Configure {
    pb::Configure {
        script_name: request.script_name,
        limits: request.limits.map(|limits| pb::ResourceLimits {
            max_duration_micros: limits.max_duration_micros,
            max_memory_bytes: limits.max_memory_bytes,
            gc_interval: limits.gc_interval,
            max_recursion_depth: limits.max_recursion_depth,
        }),
        type_check: request.type_check,
        type_check_stubs: request.type_check_stubs,
        monty_version: MONTY_VERSION.to_owned(),
        assert_message_annotations: request.assert_message_annotations,
    }
}

/// Converts a host call outcome into the child state machine's result type.
fn call_result_from_component(result: CallResult) -> Result<pb::ExtFunctionResult, String> {
    let kind = match result {
        CallResult::ReturnValue(value) => {
            pb::ext_function_result::Kind::ReturnValue(value::from_component(value)?.into())
        }
        CallResult::Error(error) => pb::ext_function_result::Kind::Error(pb::RaisedException {
            exc_type: error.exc_type,
            message: Some(error.message),
            traceback: vec![],
            data: None,
        }),
        CallResult::PendingFuture(call_id) => pb::ext_function_result::Kind::Future(call_id),
        CallResult::NotFound(name) => pb::ext_function_result::Kind::NotFound(name),
        CallResult::NotHandled => pb::ext_function_result::Kind::NotHandled(pb::Unit {}),
    };
    Ok(pb::ExtFunctionResult { kind: Some(kind) })
}

/// Converts one child event into its semantic component representation.
fn event_from_proto(event: pb::ChildEvent) -> Event {
    match event.kind {
        Some(pb::child_event::Kind::Print(print)) => Event::Print(PrintEvent {
            stderr: print.stream == i32::from(pb::PrintStream::Stderr),
            text: print.text,
        }),
        Some(pb::child_event::Kind::FunctionCall(call)) => Event::FunctionCall(FunctionCallEvent {
            function_name: call.function_name,
            args: call.args.into_iter().map(value::into_component).collect(),
            kwargs: call
                .kwargs
                .into_iter()
                .map(|(key, value)| ValuePair {
                    key: value::into_component(key),
                    value: value::into_component(value),
                })
                .collect(),
            call_id: call.call_id,
            method_call: call.method_call,
        }),
        Some(pb::child_event::Kind::OsCall(call)) => os_event_from_proto(call),
        Some(pb::child_event::Kind::NameLookup(lookup)) => Event::NameLookup(lookup.name),
        Some(pb::child_event::Kind::ResolveFutures(futures)) => Event::ResolveFutures(futures.pending_call_ids),
        Some(pb::child_event::Kind::Complete(complete)) => complete
            .value
            .and_then(|value| value.0)
            .map(value::into_component)
            .map_or_else(|| invalid_event("Complete event carried no value"), Event::Complete),
        Some(pb::child_event::Kind::Error(error)) => error
            .exception
            .map(exception_from_proto)
            .map_or_else(|| invalid_event("Error event carried no exception"), Event::Error),
        Some(pb::child_event::Kind::TypingError(error)) => Event::TypingError(error.diagnostics),
        Some(pb::child_event::Kind::DumpResult(result)) => Event::DumpResult(result.state),
        Some(pb::child_event::Kind::Ok(_)) => Event::Ok,
        Some(pb::child_event::Kind::FatalError(error)) => Event::FatalError(error.message),
        Some(pb::child_event::Kind::Shutdown(shutdown)) => Event::Shutdown(shutdown.dump),
        None => invalid_event("ChildEvent carried no kind"),
    }
}

/// Projects a typed protocol OS call into the public callback shape.
fn os_event_from_proto(call: pb::OsCall) -> Event {
    let call_id = call.call_id;
    match call.call.map(OsFunctionCall::try_from) {
        Some(Ok(call)) => {
            let function_name = call.name().to_owned();
            let (args, kwargs) = call.to_args();
            Event::OsCall(OsCallEvent {
                function_name,
                args: args.into_iter().map(value::into_component).collect(),
                kwargs: kwargs
                    .into_iter()
                    .map(|(key, value)| ValuePair {
                        key: value::into_component(key),
                        value: value::into_component(value),
                    })
                    .collect(),
                call_id,
            })
        }
        Some(Err(error)) => invalid_event(&format!("invalid OS call: {error}")),
        None => invalid_event("OsCall carried no call"),
    }
}

/// Converts a protocol exception and renders its canonical traceback once.
fn exception_from_proto(exception: pb::RaisedException) -> RaisedException {
    match MontyException::try_from(exception) {
        Ok(exception) => RaisedException {
            exc_type: exception.exc_type().to_string(),
            message: exception.message().unwrap_or("").to_owned(),
            traceback: exception.to_string(),
            frames: exception
                .traceback()
                .iter()
                .map(|frame| StackFrame {
                    filename: frame.filename.clone(),
                    line: frame.start.line,
                    column: frame.start.column,
                    end_line: frame.end.line,
                    end_column: frame.end.column,
                    frame_name: frame.frame_name.clone(),
                    preview_line: frame.preview_line.as_ref().map(ToString::to_string),
                    hide_caret: frame.hide_caret,
                    hide_frame_name: frame.hide_frame_name,
                })
                .collect(),
        },
        Err(error) => RaisedException {
            exc_type: ExcType::RuntimeError.to_string(),
            message: format!("invalid exception from worker: {error}"),
            traceback: format!("RuntimeError: invalid exception from worker: {error}"),
            frames: vec![],
        },
    }
}

/// Creates a fatal semantic event for an impossible child output shape.
fn invalid_event(message: &str) -> Event {
    Event::FatalError(format!("worker produced a malformed event: {message}"))
}

bindings::export!(Component with_types_in bindings);
