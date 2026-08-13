use std::sync::Arc;

use super::EnvFilter;
use crate::Server;

/// Temporarily suppresses the console log subscriber layer.
///
/// Construction replaces the console filter with an empty filter and retains a
/// restoration filter. Dropping the guard restores that value.
pub struct Suppress {
	server: Arc<Server>,
	restore: EnvFilter,
}

impl Suppress {
	/// Suppresses console logging until the returned guard is dropped.
	///
	/// The current console filter is saved when available; otherwise one is
	/// rebuilt from the configured directives. The stored filter and cloned
	/// server handle let `Drop` reach the reload map and restore logging.
	///
	/// # Panics
	///
	/// Panics if the shared reload-handle map mutex is poisoned.
	pub fn new(server: &Arc<Server>) -> Self {
		let handle = "console";
		let config = &server.config.log;
		let suppress = EnvFilter::default();
		let restore = server
			.log
			.reload
			.current(handle)
			.unwrap_or_else(|| EnvFilter::try_new(config).unwrap_or_default());

		server
			.log
			.reload
			.reload(&suppress, Some(&[handle]))
			.expect("log filter reloaded");

		Self { server: server.clone(), restore }
	}
}

impl Drop for Suppress {
	fn drop(&mut self) {
		self.server
			.log
			.reload
			.reload(&self.restore, Some(&["console"]))
			.expect("log filter reloaded");
	}
}
