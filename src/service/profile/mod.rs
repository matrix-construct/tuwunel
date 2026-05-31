use std::{collections::BTreeMap, sync::Arc};

use futures::{Stream, StreamExt, future::join};
use ruma::{
	MxcUri, OwnedMxcUri, OwnedRoomId, RoomId, UserId,
	api::federation::query::get_profile_information,
	events::room::member::{MembershipState, RoomMemberEventContent},
	profile::{ProfileFieldName, ProfileFieldValue},
};
use serde::Deserialize;
use serde_json::Value;
use tuwunel_core::{
	Err, Result, err, extract_variant, implement,
	matrix::PduBuilder,
	utils::{
		TryReadyExt,
		future::TryExtExt,
		stream::{IterStream, TryIgnore, automatic_width},
	},
	warn,
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, Map};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	useridprofilekey_value: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			useridprofilekey_value: args.db["useridprofilekey_value"].clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Per-update policy for fanning a global profile change out to each of
/// the user's joined rooms as a fresh `m.room.member` event. Mirrors the
/// MSC4466 `propagate_to` axis.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Propagation {
	/// Send a member event to every joined room.
	All,

	/// Send a member event only to rooms whose current per-room value
	/// matches the user's prior global value; rooms with a per-room
	/// override (e.g. set via `/myroomnick`) are skipped.
	Unchanged,

	/// Send no member events; update the global profile only.
	None,
}

#[implement(Service)]
pub async fn update_all_rooms(
	&self,
	user_id: &UserId,
	profile_values: &[(ProfileFieldName, Option<Value>)],
	propagation: Propagation,
) {
	if matches!(propagation, Propagation::None) {
		return;
	}

	let rooms: Vec<OwnedRoomId> = self
		.services
		.state_cache
		.rooms_joined(user_id)
		.map(Into::into)
		.collect()
		.await;

	rooms
		.iter()
		.stream()
		.for_each_concurrent(automatic_width(), async |room_id| {
			if let Err(e) = self
				.update_room(user_id, room_id, profile_values, propagation)
				.await
			{
				warn!(
					%user_id,
					%room_id,
					%e,
					"Failed to update room profile",
				);
			}
		})
		.await;
}

#[implement(Service)]
async fn update_room(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	profile_values: &[(ProfileFieldName, Option<Value>)],
	propagation: Propagation,
) -> Result {
	let unchanged = match propagation {
		| Propagation::All => false,
		| Propagation::Unchanged => true,
		| Propagation::None => return Ok(()),
	};

	let mut content = self
		.services
		.state_accessor
		.get_member(room_id, user_id)
		.await?;

	if !matches!(content.membership, MembershipState::Join) {
		return Ok(());
	}

	let mut changed = false;

	for (name, value) in profile_values {
		match name {
			| ProfileFieldName::DisplayName => {
				if unchanged {
					let current_displayname = self.displayname(user_id).ok().await;

					if content.displayname != current_displayname {
						continue;
					}
				}

				let displayname = value.clone().map(|value| {
					extract_variant!(value, Value::String).expect("invalid profile value type")
				});

				content.displayname = displayname;

				changed = true;
			},
			| ProfileFieldName::AvatarUrl => {
				if unchanged {
					let current_avatar_url = self.avatar_url(user_id).ok().await;

					if content.avatar_url != current_avatar_url {
						continue;
					}
				}

				let avatar_url = value.clone().map(|value| {
					serde_json::from_value(value).expect("invalid profile value type")
				});

				content.avatar_url = avatar_url;

				changed = true;
			},
			| _ => {},
		}
	}

	if !changed {
		return Ok(());
	}

	content.reason = None;

	let state_lock = self.services.state.mutex.lock(room_id).await;

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(user_id.as_str(), &content),
			user_id,
			room_id,
			&state_lock,
		)
		.await?;

	Ok(())
}

/// Sets a new displayname or removes it if displayname is None. You still
/// need to notify all rooms of this change.
#[implement(Service)]
pub async fn set_displayname(
	&self,
	user_id: &UserId,
	displayname: Option<&str>,
	propagation: Option<Propagation>,
) -> Result {
	self.set_profile_keys(
		user_id,
		&[(
			ProfileFieldName::DisplayName,
			displayname.map(|displayname| {
				serde_json::to_value(displayname).expect("displayname serialization cannot fail")
			}),
		)],
		propagation,
	)
	.await
}

/// Returns the displayname of a user on this homeserver.
#[implement(Service)]
pub async fn displayname(&self, user_id: &UserId) -> Result<String> {
	self.profile_key(user_id, &ProfileFieldName::DisplayName)
		.await
}

/// Sets a new avatar_url or removes it if avatar_url is None.
#[implement(Service)]
pub async fn set_avatar_url(
	&self,
	user_id: &UserId,
	avatar_url: Option<&MxcUri>,
	propagation: Option<Propagation>,
) -> Result {
	self.set_profile_keys(
		user_id,
		&[(
			ProfileFieldName::AvatarUrl,
			avatar_url.map(|avatar_url| {
				serde_json::to_value(avatar_url).expect("avatar url serialization cannot fail")
			}),
		)],
		propagation,
	)
	.await
}

/// Get the `avatar_url` of a user.
#[implement(Service)]
pub async fn avatar_url(&self, user_id: &UserId) -> Result<OwnedMxcUri> {
	self.profile_key(user_id, &ProfileFieldName::AvatarUrl)
		.await
}

/// Sets a new timezone or removes it if timezone is None.
#[implement(Service)]
pub async fn set_timezone(
	&self,
	user_id: &UserId,
	timezone: Option<&str>,
	propagation: Option<Propagation>,
) -> Result {
	self.set_profile_keys(
		user_id,
		&[(
			ProfileFieldName::TimeZone,
			timezone.map(|timezone| {
				serde_json::to_value(timezone).expect("timezone serialization cannot fail")
			}),
		)],
		propagation,
	)
	.await
}

/// Get the timezone of a user.
#[implement(Service)]
pub async fn timezone(&self, user_id: &UserId) -> Result<String> {
	self.profile_key(user_id, &ProfileFieldName::TimeZone)
		.await
}

/// Gets all the user's profile keys and values in an iterator
#[implement(Service)]
pub fn all_profile_keys(&self, user_id: &UserId) -> impl Stream<Item = ProfileFieldValue> + Send {
	let prefix = (user_id, Interfix);
	self.useridprofilekey_value
		.stream_prefix(&prefix)
		.ignore_err()
		.map(move |((_, key), Json(val)): ((Ignore, _), _)| {
			ProfileFieldValue::new(key, val).map_err(|_| {
				err!(Database(
					error!(%user_id, %key, "Invalid json in database profile value while iterating")
				))
			})
		})
		.ignore_err()
}

#[implement(Service)]
pub async fn clear_profile_keys(&self, user_id: &UserId) {
	let prefix = (user_id, Interfix);

	self.useridprofilekey_value
		.keys_prefix_raw(&prefix)
		.ready_try_for_each(|key| {
			self.useridprofilekey_value.remove(key);
			Ok(())
		})
		.await
		.ok();
}

/// Sets new profile key values, removes the key if value is None
#[implement(Service)]
pub async fn set_profile_keys(
	&self,
	user_id: &UserId,
	profile_values: &[(ProfileFieldName, Option<Value>)],
	propagation: Option<Propagation>,
) -> Result {
	if self.services.globals.user_is_local(user_id) {
		for (name, value) in profile_values {
			check_profile_key(name.as_str())?;

			if let Some(value) = value {
				self.enforce_profile_size(user_id, name.as_str(), value)
					.await?;
			}
		}
	}

	let propagation = propagation.unwrap_or(
		if self
			.services
			.config
			.preserve_room_profile_overrides
		{
			Propagation::Unchanged
		} else {
			Propagation::All
		},
	);

	if !matches!(propagation, Propagation::None) {
		assert!(
			self.services.globals.user_is_local(user_id),
			"propagation requested for remote user"
		);

		self.update_all_rooms(user_id, profile_values, propagation)
			.await;
	}

	for (name, value) in profile_values {
		let key = (user_id, name.as_str());

		if let Some(value) = value {
			self.useridprofilekey_value.put(key, Json(value));
		} else {
			self.useridprofilekey_value.del(key);
		}
	}

	Ok(())
}

/// Gets a specific user profile key
#[implement(Service)]
pub async fn profile_key<T>(&self, user_id: &UserId, profile_key: &ProfileFieldName) -> Result<T>
where
	T: for<'de> Deserialize<'de> + Send,
{
	let key = (user_id, profile_key);
	let Json(value) = self
		.useridprofilekey_value
		.qry(&key)
		.await
		.map_err(|_| err!(Request(NotFound("The requested profile key does not exist."))))?
		.deserialized()
		.map_err(|_| err!(Database("Cannot deserialize database profile value")))?;

	Ok(value)
}

#[implement(Service)]
pub async fn fill_profile_data(&self, user_id: &UserId, content: &mut RoomMemberEventContent) {
	let displayname = self.displayname(user_id).ok();
	let avatar_url = self.avatar_url(user_id).ok();

	let (displayname, avatar_url) = join(displayname, avatar_url).await;

	content.displayname = displayname;
	content.avatar_url = avatar_url;
}

#[implement(Service)]
pub async fn fetch_remote_profile(&self, user_id: &UserId) -> Result {
	assert!(
		!self.services.globals.user_is_local(user_id),
		"fetch remote profile called with a local user"
	);

	if let Ok(response) = self
		.services
		.federation
		.execute(user_id.server_name(), get_profile_information::v1::Request {
			user_id: user_id.to_owned(),
			field: None,
		})
		.await
	{
		if !self.services.users.exists(user_id).await {
			self.services
				.users
				.create(user_id, None, None)
				.await?;
		}

		for (key, value) in response.iter() {
			self.set_profile_keys(
				user_id,
				&[(key.as_str().into(), Some(value.clone()))],
				Some(Propagation::None),
			)
			.await?;
		}
	}

	Ok(())
}

/// MSC4133 maximum total profile size (64 KiB), measured over the JSON of the
/// full profile including displayname and avatar_url.
pub(super) const MAX_PROFILE_SIZE: usize = 65_536;

/// MSC4133: reject a prospective profile write that would push the full
/// profile over the 64 KiB cap. `value` is what `key` will hold after the
/// write; a removal cannot grow the profile, so callers skip it.
#[implement(Service)]
async fn enforce_profile_size(&self, user_id: &UserId, key: &str, value: &Value) -> Result {
	let mut profile: BTreeMap<_, _> = self
		.all_profile_keys(user_id)
		.map(|profile_value| {
			(
				profile_value.field_name().as_str().to_owned(),
				profile_value.value().into_owned(),
			)
		})
		.collect()
		.await;
	profile.insert(key.to_owned(), value.clone());

	let profile_size = serde_json::to_vec(&profile).map_or(0, |buf| buf.len());

	if profile_size > MAX_PROFILE_SIZE {
		return Err!(Request(ProfileTooLarge(
			"Profile would exceed the maximum size of 64 KiB."
		)));
	}

	Ok(())
}

/// MSC4133 maximum profile field-name length, in bytes.
const MAX_KEY_LENGTH: usize = 255;

/// Validate a profile field name against the Common Namespaced Identifier
/// Grammar: a lowercase-leading identifier over `[a-z0-9_.-]`, matching the
/// reference homeserver. Length is bounded separately by `MAX_KEY_LENGTH`.
fn check_profile_key(name: &str) -> Result {
	if name.len() > MAX_KEY_LENGTH {
		return Err!(Request(KeyTooLarge("Profile key names cannot be longer than 255 bytes.")));
	}

	let ok = name
		.bytes()
		.next()
		.is_some_and(|b| b.is_ascii_lowercase())
		&& name.bytes().all(|b| {
			b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
		});

	if !ok {
		return Err!(Request(BadJson(
			"Profile key names must follow the Common Namespaced Identifier Grammar."
		)));
	}

	Ok(())
}
