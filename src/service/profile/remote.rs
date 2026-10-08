use std::borrow::Cow;

use futures::{StreamExt, TryStreamExt};
use ruma::{
	UserId,
	api::federation::query::get_profile_information::v1::{Request, Response},
	profile::ProfileFieldName,
};
use serde_json::Value;
use tuwunel_core::{
	Err, Result, implement,
	smallvec::SmallVec,
	utils::{
		json::serialized_len,
		stream::{IterStream, TryReadyExt},
	},
};

use super::{MAX_PROFILE_SIZE, Propagation, ProspectiveProfile, Service, check_profile_key};

type Removed = SmallVec<[ProfileFieldName; 1]>;

type Fields = SmallVec<[(ProfileFieldName, Option<Value>); 2]>;

/// Merges a served profile without inferring deletions from omitted fields.
///
/// The response and retained cache are checked before creating an unknown user
/// or publishing fields. One locked batch commits every changed field together.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all, fields(%user_id))]
pub(super) async fn merge_profile(&self, user_id: &UserId, response: Response) -> Result {
	let max_fields = self.services.config.max_remote_profile_fields;

	check_served_profile(&response, max_fields)?;

	let profile_lock = self.mutex.lock(user_id).await;
	let incoming = response
		.iter()
		.map(|(name, value)| (name.as_str().into(), Cow::Borrowed(value)))
		.try_stream();

	let prospective: ProspectiveProfile<'_> = self
		.try_all_profile_keys(user_id)
		.map_ok(|field| (field.field_name(), Cow::Owned(field.value().into_owned())))
		.chain(incoming)
		.try_collect()
		.await?;

	check_field_count(prospective.len(), max_fields)?;

	prospective
		.keys()
		.try_for_each(|name| check_profile_key(name.as_str()))?;

	if serialized_len(&prospective)? > MAX_PROFILE_SIZE {
		return Err!(Request(ProfileTooLarge(
			"Profile would exceed the maximum size of 64 KiB."
		)));
	}

	if !self.services.users.exists(user_id).await {
		self.services
			.users
			.create(user_id, None, None)
			.await?;
	}

	let fields: Fields = response
		.into_iter()
		.map(|(name, value)| (name.into(), Some(value)))
		.collect();

	self.set_profile_keys_locked(&profile_lock, user_id, &fields, Some(Propagation::None))
		.await
}

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
	check_served_profile(&response, self.services.config.max_remote_profile_fields)?;

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

fn check_served_profile(response: &Response, max_fields: usize) -> Result {
	check_field_count(response.iter().count(), max_fields)?;

	response
		.iter()
		.try_for_each(|(name, _)| check_profile_key(name))?;

	if serialized_len(&response.data)? > MAX_PROFILE_SIZE {
		return Err!(Request(ProfileTooLarge("Profile exceeds the maximum size of 64 KiB.")));
	}

	Ok(())
}

fn check_field_count(count: usize, maximum: usize) -> Result {
	if count > maximum {
		return Err!(Request(ProfileTooLarge("Remote profile exceeds the field-count limit.")));
	}

	Ok(())
}
