use tracing_subscriber::EnvFilter;
use tuwunel_core::{Result, err};

use crate::admin_command;

#[admin_command]
pub(super) async fn change_log_level(&self, filter: Option<String>, reset: bool) -> Result {
	let handles = &["console"];

	let filter = reset
		.then_some(&self.services.config.logging.filter)
		.or(filter.as_ref())
		.ok_or_else(|| err!("No log level was specified."))?;

	let filter_layer = EnvFilter::try_new(filter).map_err(|e| {
		let source = if !reset { "specified" } else { "found in config" };
		err!("Invalid log level filter {source}: {e}")
	})?;

	self.services
		.server
		.log
		.reload
		.reload(&filter_layer, Some(handles))
		.map_err(|e| err!("Failed to modify and reload the global tracing log level: {e}"))?;

	write!(self, "Successfully changed log level to {filter}").await
}
