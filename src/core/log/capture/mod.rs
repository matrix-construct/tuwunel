/// Captured tracing-event data and common accessors.
///
/// Values borrow the event, current span, and fields for one filter or callback
/// invocation. They must not escape that invocation.
pub mod data;
mod guard;
/// Tracing subscriber layer for ephemeral event captures.
///
/// The layer selects active captures, records event fields, and invokes their
/// callbacks synchronously.
pub mod layer;
/// Shared registration state for active captures.
///
/// The state coordinates capture guards with the subscriber layer.
/// Registrations are reference counted and protected for concurrent access.
pub mod state;
/// Callback constructors for formatting captured log events.
///
/// Helpers append HTML or Markdown lines to a shared formatter. A generic
/// constructor accepts compatible formatting functions.
pub mod util;

use std::sync::{Arc, Mutex};

pub use data::Data;
pub use guard::Guard;
pub use layer::{Layer, Value};
pub use state::State;
pub use util::*;

/// Predicate used to select tracing events for a capture.
///
/// Returning true delivers the event to the capture callback. Filters must be
/// thread safe because tracing events can originate on any thread. They execute
/// while registration state is read-locked and must not mutate captures on the
/// same state.
pub type Filter = dyn Fn(Data<'_>) -> bool + Send + Sync + 'static;

/// Callback invoked for each tracing event selected by a capture.
///
/// The callback is serialized by the owning capture so mutable state can be
/// updated safely. It executes while registration state is read-locked and must
/// not mutate captures on the same state.
pub type Closure = dyn FnMut(Data<'_>) + Send + Sync + 'static;

/// Capture instance state.
pub struct Capture {
	state: Arc<State>,
	filter: Option<Box<Filter>>,
	closure: Mutex<Box<Closure>>,
}

impl Capture {
	/// Construct a new capture instance. Capture does not start until the Guard
	/// is in scope.
	#[must_use]
	pub fn new<F, C>(state: &Arc<State>, filter: Option<F>, closure: C) -> Arc<Self>
	where
		F: Fn(Data<'_>) -> bool + Send + Sync + 'static,
		C: FnMut(Data<'_>) + Send + Sync + 'static,
	{
		Arc::new(Self {
			state: state.clone(),
			filter: filter.map(|p| -> Box<Filter> { Box::new(p) }),
			closure: Mutex::new(Box::new(closure)),
		})
	}

	/// Creates one active registration for the lifetime of a scope guard.
	///
	/// Registration happens before the guard is returned. Multiple guards can
	/// register the same capture, and dropping each guard removes its own
	/// registration.
	#[must_use]
	pub fn start(self: &Arc<Self>) -> Guard {
		self.state.add(self);
		Guard { capture: self.clone() }
	}

	/// Removes one active registration for this capture.
	///
	/// Calling the method for an inactive capture has no effect. Other
	/// registrations of the same capture remain active, and a callback already
	/// in progress can finish before removal becomes observable.
	pub fn stop(self: &Arc<Self>) { self.state.del(self); }
}
