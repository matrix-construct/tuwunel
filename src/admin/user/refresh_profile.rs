use ruma::{OwnedUserId, profile::ProfileFieldName};
use tuwunel_core::{Result, itertools::Itertools};

use crate::{admin_command, utils::check_known_remote_user};

#[admin_command]
pub(super) async fn refresh_profile(&self, user_id: OwnedUserId) -> Result {
	check_known_remote_user(self.services, &user_id).await?;

	let removed = self
		.services
		.profile
		.mirror_remote_profile(&user_id)
		.await?;

	if removed.is_empty() {
		return write!(self, "Refreshed the profile of {user_id}; no cached field was stale.")
			.await;
	}

	let names = removed
		.iter()
		.map(ProfileFieldName::as_str)
		.format(", ");

	write!(self, "Refreshed the profile of {user_id}; removed {names}.").await
}
