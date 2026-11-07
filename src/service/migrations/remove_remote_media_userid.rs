use tuwunel_core::{Result, info, utils::TryReadyExt, warn};
use tuwunel_database::Map;

use crate::{
	Services,
	integrity::mediaid_user::{Remote, remote},
};

pub(super) async fn remove_remote_media_userid(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let mediaid_user = &db["mediaid_user"];

	warn!("Removing stored user id for remote media");

	let (removed_remote, removed_invalid, unparsable) = remote(services)
		.ready_try_fold((0_usize, 0_usize, 0_usize), |totals, finding| {
			Ok(repair(mediaid_user, totals, &finding))
		})
		.await?;

	drop(cork);
	info!(
		%removed_remote,
		%removed_invalid,
		%unparsable,
		"Removed stored user id for remote media"
	);

	mediaid_user.sort()
}

fn repair(
	mediaid_user: &Map,
	(removed_remote, removed_invalid, unparsable): (usize, usize, usize),
	finding: &Remote,
) -> (usize, usize, usize) {
	match finding {
		| Remote::Unparsable { .. } =>
			(removed_remote, removed_invalid, unparsable.saturating_add(1)),
		| Remote::Invalid { key, mxc } => {
			warn!(?mxc, "Invalid MXC URL, removing it");

			mediaid_user.remove(key);

			(removed_remote, removed_invalid.saturating_add(1), unparsable)
		},
		| Remote::Foreign { key, .. } => {
			mediaid_user.remove(key);

			(removed_remote.saturating_add(1), removed_invalid, unparsable)
		},
	}
}
