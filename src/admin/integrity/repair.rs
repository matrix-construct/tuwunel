use tuwunel_core::{Err, Result, itertools::Itertools};
use tuwunel_service::migrations::{Stamp, rerun};

use crate::admin_command;

#[admin_command]
pub(super) async fn integrity_repair(
	&self,
	name: String,
	yes_i_know_what_im_doing: bool,
) -> Result {
	if !yes_i_know_what_im_doing {
		return Err!(
			"Pass --yes-i-know-what-im-doing to clear the stamps, which makes the next start \
			 run the repair of {name} again."
		);
	}

	let stamps = rerun(self.services, &name).await?;
	let listing = stamps.format_with("", |Stamp { marker, cleared }, entry| {
		let state = if cleared { "cleared" } else { "already pending" };

		entry(&format_args!("- {marker}: {state}\n"))
	});

	self.write_string(format!(
		"The next start with database_migrations enabled runs the repair of {name}:\n{listing}"
	))
	.await
}
