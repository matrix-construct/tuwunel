#[cfg(test)]
mod tests;

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
	smallvec::SmallVec,
	utils::{
		ReadyExt, TryReadyExt,
		future::TryExtExt,
		stream::{IterStream, TryIgnore, automatic_width},
	},
	warn,
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Json, KeyVal, Map};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	profilechangeid_userid: Arc<Map>,
	useridprofilekey_value: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			profilechangeid_userid: args.db["profilechangeid_userid"].clone(),
			useridprofilekey_value: args.db["useridprofilekey_value"].clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// One logged profile write: the user whose profile changed, and the name of
/// the field that changed.
///
/// Both members borrow the database cursor that produced them, so a consumer
/// retaining either past the cursor's next advance must own it first.
pub type ProfileChange<'a> = (&'a UserId, &'a str);

/// A row of the profile change log: the field's name rides the key so that a
/// write covering several fields needs one count, and the value names the user
/// for the rows keyed by room.
type ChangeKeyVal<'a> = KeyVal<'a, (&'a str, u64, &'a str), &'a UserId>;

/// The field names one write actually changes, almost always just the one the
/// caller named.
type ChangedFields<'a> = SmallVec<[&'a str; 1]>;

/// MSC4426 maximum `m.status` text length, in bytes.
const MAX_STATUS_TEXT_LENGTH: usize = 256;

/// MSC4426 maximum `m.status` emoji length, in bytes.
const MAX_STATUS_EMOJI_LENGTH: usize = 32;

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

	if !profile_values.iter().any(|(name, _)| {
		matches!(name, ProfileFieldName::DisplayName | ProfileFieldName::AvatarUrl)
	}) {
		return;
	}

	// Suspended senders may not emit member events; OIDC, SSO, and MAS profile
	// updates reach here without passing any suspension-blocked route.
	if self.services.users.is_suspended(user_id).await {
		return;
	}

	let (current_displayname, current_avatar_url) =
		if matches!(propagation, Propagation::Unchanged) {
			join(self.displayname(user_id).ok(), self.avatar_url(user_id).ok()).await
		} else {
			(None, None)
		};

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
				.update_room(
					user_id,
					room_id,
					profile_values,
					propagation,
					current_displayname.as_deref(),
					current_avatar_url.as_deref(),
				)
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
	current_displayname: Option<&str>,
	current_avatar_url: Option<&MxcUri>,
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
				if unchanged && content.displayname.as_deref() != current_displayname {
					continue;
				}

				let displayname = value.clone().map(|value| {
					extract_variant!(value, Value::String).expect("invalid profile value type")
				});

				content.displayname = displayname;

				changed = true;
			},
			| ProfileFieldName::AvatarUrl => {
				if unchanged && content.avatar_url.as_deref() != current_avatar_url {
					continue;
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
				check_profile_value(name.as_str(), value)?;
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

	if !matches!(propagation, Propagation::None) && self.services.globals.user_is_local(user_id) {
		self.update_all_rooms(user_id, profile_values, propagation)
			.await;
	}

	let changed = self.changed_fields(user_id, profile_values).await;

	for (name, value) in profile_values {
		let key = (user_id, name.as_str());

		if let Some(value) = value {
			self.useridprofilekey_value.put(key, Json(value));
		} else {
			self.useridprofilekey_value.del(key);
		}
	}

	self.mark_profile_update(user_id, &changed).await;

	Ok(())
}

/// Names the fields whose stored value the write would actually change.
///
/// A profile write that restores what is already stored is a change to nobody,
/// and the on-demand remote refresh reissues every field on every lookup of a
/// remote profile, so logging those would multiply the log by the request rate
/// rather than the change rate.
#[implement(Service)]
async fn changed_fields<'a>(
	&self,
	user_id: &UserId,
	profile_values: &'a [(ProfileFieldName, Option<Value>)],
) -> ChangedFields<'a> {
	profile_values
		.iter()
		.stream()
		.filter_map(async |(name, value)| {
			let stored: Option<Value> = self.profile_key(user_id, name).await.ok();

			stored
				.as_ref()
				.ne(&value.as_ref())
				.then_some(name.as_str())
		})
		.collect()
		.await
}

/// Records a profile write under the user's own prefix and under every room
/// they are joined to.
///
/// The key names the changed field and not only the user because a removal is
/// otherwise unreportable: a reader re-reading the live profile cannot tell a
/// cleared field from one that was never set. Remote users are logged too, but
/// only as fresh as the on-demand fetch that replaced their stored fields,
/// since nothing pushes a remote profile change to us.
#[implement(Service)]
#[tracing::instrument(
	name = "profile_update",
	level = "debug",
	skip_all,
	fields(
		%user_id,
	),
)]
async fn mark_profile_update(&self, user_id: &UserId, changed: &[&str]) {
	if changed.is_empty() {
		return;
	}

	let count = self.services.globals.next_count();

	for name in changed {
		self.profilechangeid_userid
			.put_raw((user_id, *count, name), user_id);
	}

	self.services
		.state_cache
		.rooms_joined(user_id)
		.ready_for_each(|room_id| {
			for name in changed {
				self.profilechangeid_userid
					.put_raw((room_id, *count, name), user_id);
			}
		})
		.await;
}

/// Streams the profile fields the user changed themselves.
///
/// The range is half-open on the low side, so a caller passes the sync token
/// it already delivered. An absent `to` leaves the walk unbounded above.
#[implement(Service)]
#[inline]
pub fn profile_changed<'a>(
	&'a self,
	user_id: &'a UserId,
	from: u64,
	to: Option<u64>,
) -> impl Stream<Item = ProfileChange<'a>> + Send + 'a {
	self.profile_changed_user_or_room(user_id.as_str(), from, to)
}

/// Streams the profile fields any member of the room changed.
///
/// The range works as it does for a single user. A member appears once per
/// field they changed, however many of the caller's rooms they share.
#[implement(Service)]
#[inline]
pub fn room_profile_changed<'a>(
	&'a self,
	room_id: &'a RoomId,
	from: u64,
	to: Option<u64>,
) -> impl Stream<Item = ProfileChange<'a>> + Send + 'a {
	self.profile_changed_user_or_room(room_id.as_str(), from, to)
}

#[implement(Service)]
fn profile_changed_user_or_room<'a>(
	&'a self,
	user_or_room_id: &'a str,
	from: u64,
	to: Option<u64>,
) -> impl Stream<Item = ProfileChange<'a>> + Send + 'a {
	let to = to.unwrap_or(u64::MAX);
	let start = (user_or_room_id, from.saturating_add(1));

	// User and room ids never collide as a prefix here: their sigils differ.
	self.profilechangeid_userid
		.stream_from(&start)
		.ignore_err()
		.ready_take_while(move |((prefix, count, _), _): &ChangeKeyVal<'_>| {
			*prefix == user_or_room_id && *count <= to
		})
		.map(|((_, _, field), user_id): ChangeKeyVal<'_>| (user_id, field))
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
		.map_err(|error| {
			if error.is_not_found() {
				err!(Request(NotFound("The requested profile key does not exist.")))
			} else {
				error
			}
		})?
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

/// Validate a profile field value against the schema of the proposal naming
/// the field.
///
/// MSC4133 reserves no schema of its own, so a field this does not recognize
/// carries any JSON the size cap admits. A stored `null` clears the field for
/// readers without removing it, and is accepted for every field name.
fn check_profile_value(name: &str, value: &Value) -> Result {
	if value.is_null() {
		return Ok(());
	}

	match name {
		| "m.status" | "org.matrix.msc4426.status" => check_status(value),
		| "m.call" | "org.matrix.msc4426.call" => check_call(value),
		| "m.tz" | "us.cloke.msc4175.tz" => check_timezone(value),
		| _ => Ok(()),
	}
}

/// Validate an MSC4426 status against its two required fields and their byte
/// budgets.
///
/// Both `text` and `emoji` are required, so a partial object is rejected
/// rather than stored for clients to render half of. The emoji budget counts
/// bytes and never graphemes, which the proposal calls out because grapheme
/// definitions keep growing.
fn check_status(value: &Value) -> Result {
	let (Some(text), Some(emoji)) = (
		value.get("text").and_then(Value::as_str),
		value.get("emoji").and_then(Value::as_str),
	) else {
		return Err!(Request(BadJson("Status requires a text and an emoji string.")));
	};

	check_status_length(text, MAX_STATUS_TEXT_LENGTH, "text")?;
	check_status_length(emoji, MAX_STATUS_EMOJI_LENGTH, "emoji")
}

/// Bound one status field by its byte budget.
///
/// MSC4426 mandates both the `M_TOO_LARGE` errcode and a 400, while the
/// kind-derived table promotes that errcode to 413, so this is one of the few
/// call sites naming its own status.
fn check_status_length(value: &str, max: usize, field: &str) -> Result {
	value.len().le(&max).then_some(()).ok_or_else(|| {
		err!(RequestStatus(
			BAD_REQUEST,
			TooLarge("Status {field} cannot be longer than {max} bytes.")
		))
	})
}

/// Validate an MSC4426 call indicator.
///
/// Every field is optional, so an empty object is the valid "in a call, joined
/// at an unstated time" value the proposal's own example uses.
fn check_call(value: &Value) -> Result {
	let Some(call) = value.as_object() else {
		return Err!(Request(BadJson("Call must be an object.")));
	};

	call.get("call_joined_ts")
		.is_none_or(Value::is_number)
		.then_some(())
		.ok_or_else(|| err!(Request(BadJson("Call join timestamp must be a number."))))
}

/// Maximum `m.tz` length, in bytes.
///
/// The longest name the database ships is 32 bytes, so this is headroom for
/// later additions rather than a limit any real zone approaches.
const MAX_TIMEZONE_LENGTH: usize = 64;

/// Maximum number of `/`-separated components in an `m.tz` name.
///
/// The deepest names the database ships are three deep, in the
/// `America/Argentina/Buenos_Aires` shape.
const MAX_TIMEZONE_COMPONENTS: usize = 3;

/// Validate an MSC4175 time zone against the shape of an IANA Time Zone
/// Database name.
///
/// Membership in a bundled copy of the database is deliberately not tested:
/// the proposal's rationale for a loose check is that clients and servers
/// carry different database versions, so a browser one release ahead of ours
/// would have a newly added zone rejected. Shape alone still refuses offsets,
/// platform display names, and anything else that could never name a zone.
fn check_timezone(value: &Value) -> Result {
	let Some(name) = value.as_str() else {
		return Err!(Request(InvalidParam("Time zone must be a string.")));
	};

	is_timezone_name(name)
		.then_some(())
		.ok_or_else(|| {
			err!(Request(InvalidParam(
				"Time zone must be a name from the IANA Time Zone Database."
			)))
		})
}

/// Test a string against the tzfile naming rules, narrowed to what every name
/// the database ships actually uses.
///
/// A name is one to three `/`-separated components. Draining the remainder
/// afterwards is what rejects a fourth, leaving the whole test one pass over
/// the string.
fn is_timezone_name(name: &str) -> bool {
	let mut components = name.split('/');

	name.len().le(&MAX_TIMEZONE_LENGTH)
		&& components
			.by_ref()
			.take(MAX_TIMEZONE_COMPONENTS)
			.all(is_timezone_component)
		&& components.next().is_none()
}

/// Test one `/`-separated component of an `m.tz` name.
///
/// Components hold ASCII alphanumerics, `_`, `+`, and `-`. Each begins with a
/// letter, which is what refuses a bare numeric offset.
fn is_timezone_component(component: &str) -> bool {
	component
		.bytes()
		.next()
		.is_some_and(|byte| byte.is_ascii_alphabetic())
		&& component
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'-'))
}
