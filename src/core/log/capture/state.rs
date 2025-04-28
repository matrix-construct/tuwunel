use std::sync::{Arc, RwLock};

use super::Capture;

/// Capture layer state.
pub struct State {
	pub(super) active: RwLock<Vec<Arc<Capture>>>,
}

impl Default for State {
	fn default() -> Self {
		Self::new()
	}
}

impl State {
	#[must_use]
	pub fn new() -> Self {
		Self { active: RwLock::new(Vec::new()) }
	}

	pub(super) fn add(&self, capture: &Arc<Capture>) {
		self.active
			.write()
			.expect("locked for writing")
			.push(capture.clone());
	}

	pub(super) fn del(&self, capture: &Arc<Capture>) {
		let mut vec = self.active.write().expect("locked for writing");
		if let Some(pos) = vec.iter().position(|v| Arc::ptr_eq(v, capture)) {
			vec.swap_remove(pos);
		}
	}
}
