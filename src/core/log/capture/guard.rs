use std::sync::Arc;

use super::Capture;

/// Capture instance scope guard.
pub struct Guard {
	pub(super) capture: Arc<Capture>,
}

impl Drop for Guard {
	#[inline]
	fn drop(&mut self) {
		self.capture.stop();
	}
}
