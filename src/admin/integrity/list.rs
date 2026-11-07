use tuwunel_core::{Result, itertools::Itertools};
use tuwunel_service::integrity::checks;

use crate::admin_command;

#[admin_command]
pub(super) async fn integrity_list(&self) -> Result {
	let listing = checks().format_with("", |check, entry| {
		let quiescent = check
			.quiescent
			.then_some(" (read-only open)")
			.unwrap_or_default();

		entry(&format_args!("- {}{quiescent}: {}\n", check.name, check.about))
	});

	self.write_string(listing.to_string()).await
}
