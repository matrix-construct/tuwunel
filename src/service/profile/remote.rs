use futures::TryStreamExt;
use ruma::{
	UserId,
	api::federation::query::get_profile_information::v1::{Request, Response},
	profile::ProfileFieldName,
};
use serde_json::Value;
use tuwunel_core::{Result, implement, smallvec::SmallVec, utils::stream::TryReadyExt};

use super::{Propagation, Service};

type Removed = SmallVec<[ProfileFieldName; 1]>;

type Fields = Vec<(ProfileFieldName, Option<Value>)>;

/// Replaces a remote user's cached profile with the one their server serves.
///
/// Unlike `fetch_remote_profile`, which only adds and overwrites, a cached
/// field missing from the response is removed, so a value the remote user has
/// since deleted stops reaching clients. Returns the names of the removed
/// fields.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%user_id,
	),
)]
pub async fn mirror_remote_profile(&self, user_id: &UserId) -> Result<Removed> {
	assert!(
		!self.services.globals.user_is_local(user_id),
		"mirror remote profile called with a local user"
	);

	let request = Request { user_id: user_id.to_owned(), field: None };
	let response = self
		.services
		.federation
		.execute(user_id.server_name(), request)
		.await?;

	self.mirror_profile(user_id, response).await
}

/// Stores a profile response as the user's complete cached profile.
///
/// Every returned field is written and every cached field the response omits
/// is deleted in one logged write under the profile lock, so no concurrent
/// write interleaves and connected clients see the removals.
#[implement(Service)]
pub(super) async fn mirror_profile(
	&self,
	user_id: &UserId,
	response: Response,
) -> Result<Removed> {
	let profile_lock = self.mutex.lock(user_id).await;
	let removed: Removed = self
		.try_profile_field_names(user_id)
		.ready_try_filter(|name| response.get(name.as_str()).is_none())
		.try_collect()
		.await?;

	let fields: Fields = response
		.into_iter()
		.map(|(name, value)| (name.into(), Some(value)))
		.chain(removed.iter().cloned().map(|name| (name, None)))
		.collect();

	self.set_profile_keys_locked(&profile_lock, user_id, &fields, Some(Propagation::None))
		.await?;

	Ok(removed)
}
