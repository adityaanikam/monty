//! The WebAssembly Component Model Monty worker.
//!
//! This is the browser analog of `monty subprocess`: a persistent [`Child`]
//! consumes one framed protobuf request per call. The component boundary is
//! described by `wit/runtime.wit`, so generated canonical-ABI bindings carry
//! decoded event records to JavaScript without WASI stdio or a custom ABI.

use std::cell::RefCell;

use monty_proto::{
    decode_frame, pb,
    worker::{Child, HandleOutcome, dispatch_frame},
};
use pb::child_event::Kind;
use prost::Message;

#[expect(
    clippy::same_length_and_capacity,
    reason = "generated canonical-ABI list lifting uses Vec::from_raw_parts"
)]
mod bindings {
    wit_bindgen::generate!({ world: "monty-runtime" });
}

use bindings::exports::pydantic::monty::worker::{DispatchResult, Event, Guest, Status};

thread_local! {
    /// The session worker, retained for the lifetime of this component instance.
    static CHILD: RefCell<Child> = RefCell::new(Child::new());
}

/// Implements the typed component export over Monty's protocol child.
struct Component;

impl Guest for Component {
    fn dispatch(request: Vec<u8>) -> DispatchResult {
        let (reply, outcome) = CHILD.with_borrow_mut(|child| dispatch_frame(child, &request));
        if let Some(events) = decode_events(&reply) {
            let status = if matches!(outcome, HandleOutcome::Continue) {
                Status::Continue
            } else {
                Status::Shutdown
            };
            DispatchResult { status, events }
        } else {
            let fatal = pb::FatalError {
                message: "worker produced a malformed reply".to_owned(),
            };
            DispatchResult {
                status: Status::Shutdown,
                events: vec![Event {
                    kind: 11,
                    payload: fatal.encode_to_vec(),
                }],
            }
        }
    }
}

/// Decodes framed protobuf envelopes while retaining each event's typed payload.
fn decode_events(input: &[u8]) -> Option<Vec<Event>> {
    let mut events = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        let frame = next_frame(input, &mut offset)?;
        events.push(event_from_proto(decode_frame::<pb::ChildEvent>(frame).ok()?)?);
    }
    Some(events)
}

/// Returns the next length-prefixed protobuf message in `input`.
fn next_frame<'a>(input: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
    let header = input.get(*offset..*offset + 4)?;
    let len = u32::from_le_bytes(header.try_into().ok()?) as usize;
    *offset += 4;
    let frame = input.get(*offset..*offset + len)?;
    *offset += len;
    Some(frame)
}

/// Splits a child event into its semantic kind and nested message payload.
fn event_from_proto(event: pb::ChildEvent) -> Option<Event> {
    let (kind, payload) = match event.kind? {
        Kind::Print(value) => (1, value.encode_to_vec()),
        Kind::FunctionCall(value) => (2, value.encode_to_vec()),
        Kind::OsCall(value) => (3, value.encode_to_vec()),
        Kind::NameLookup(value) => (4, value.encode_to_vec()),
        Kind::ResolveFutures(value) => (5, value.encode_to_vec()),
        Kind::Complete(value) => (6, value.encode_to_vec()),
        Kind::Error(value) => (7, value.encode_to_vec()),
        Kind::TypingError(value) => (8, value.encode_to_vec()),
        Kind::DumpResult(value) => (9, value.encode_to_vec()),
        Kind::Ok(value) => (10, value.encode_to_vec()),
        Kind::FatalError(value) => (11, value.encode_to_vec()),
        Kind::Shutdown(value) => (12, value.encode_to_vec()),
    };
    Some(Event { kind, payload })
}

bindings::export!(Component with_types_in bindings);
