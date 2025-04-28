pub mod data;
mod guard;
pub mod layer;
pub mod state;
pub mod util;

use std::sync::{Arc, Mutex};

pub use data::Data;
use guard::Guard;
pub use layer::{Layer, Value};
pub use state::State;
pub use util::*;

pub type Filter = dyn Fn(Data<'_>) -> bool + Send + Sync + 'static;
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

	#[must_use]
	pub fn start(self: &Arc<Self>) -> Guard {
		self.state.add(self);
		Guard { capture: self.clone() }
	}

	pub fn stop(self: &Arc<Self>) {
		self.state.del(self);
	}
}
