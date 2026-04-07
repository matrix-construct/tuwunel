use std::{cmp::max, iter::once};

use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{
	CanonicalJsonObject, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UserId,
	api::client::room::upgrade_room::v3,
	events::{
		StateEventType, TimelineEventType,
		room::{
			create::PreviousRoom,
			member::{MembershipState, RoomMemberEventContent},
			power_levels::RoomPowerLevelsEventContent,
			tombstone::RoomTombstoneEventContent,
		},
	},
	int,
	room_version_rules::{RoomIdFormatVersion, RoomVersionRules},
};
use serde_json::{Value as JsonValue, json, value::to_raw_value};
use tuwunel_core::{
	Err, Result, debug_info, err, error, implement, info, is_equal_to, is_less_than,
	matrix::{Event, StateKey, pdu::PduBuilder, room_version},
	utils::{
		ReadyExt,
		future::TryExtExt,
		stream::{IterStream, WidebandExt},
	},
};
use tuwunel_service::{Services, rooms::timeline::RoomMutexGuard};

use crate::Ruma;

//TODO: Upgrade Ruma
const RECOMMENDED_TRANSFERABLE_STATE_EVENT_TYPES: &[StateEventType; 9] = &[
	StateEventType::RoomServerAcl,
	StateEventType::RoomEncryption,
	StateEventType::RoomName,
	StateEventType::RoomAvatar,
	StateEventType::RoomTopic,
	StateEventType::RoomGuestAccess,
	StateEventType::RoomHistoryVisibility,
	StateEventType::RoomJoinRules,
	StateEventType::RoomPowerLevels,
];

#[derive(Debug)]
struct RoomUpgradeContext<'a> {
	services: &'a Services,
	sender_user: &'a UserId,
	creator: &'a UserId,
	old_room_id: &'a RoomId,
	old_state_lock: &'a RoomMutexGuard,
	old_version_rules: &'a RoomVersionRules,
	new_room_id: &'a RoomId,
	new_state_lock: &'a RoomMutexGuard,
	new_version_rules: &'a RoomVersionRules,
	additional_creators: &'a [OwnedUserId],
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/upgrade`
///
/// Upgrades the room.
///
/// - Creates a replacement room
/// - Sends a tombstone event into the current room
/// - Sender user joins the room
/// - Transfers some state events
/// - Moves local aliases
/// - Modifies old room power levels to prevent users from speaking
#[tracing::instrument(level = "debug")]
pub(crate) async fn upgrade_room_route(
	State(services): State<crate::State>,
	body: Ruma<v3::Request>,
) -> Result<v3::Response> {
	let sender_user = body.sender_user();
	let new_version = &body.new_version;
	let version_rules = room_version::rules(new_version)?;

	if !services
		.config
		.supported_room_version(new_version)
	{
		return Err!(Request(UnsupportedRoomVersion(
			"This server does not support that room version.",
		)));
	}

	let old_room_id = &body.room_id;
	let old_state_lock = services.state.mutex.lock(old_room_id).await;

	if !services
		.state_accessor
		.user_can_tombstone(old_room_id, sender_user, &old_state_lock)
		.await
	{
		return Err!(Request(Forbidden("You are not permitted to upgrade the room.")));
	}

	let latest_event = services
		.timeline
		.latest_pdu_in_room(old_room_id)
		.await
		.ok();

	let predecessor = PreviousRoom {
		room_id: old_room_id.to_owned(),
		event_id: latest_event
			.as_ref()
			.map(Event::event_id)
			.map(ToOwned::to_owned),
	};

	debug_info!(
		%sender_user,
		%old_room_id,
		last_event = ?predecessor.event_id,
		?new_version,
		"Attempting upgrade of room..."
	);

	let creator = if services.admin.is_admin_room(&body.room_id).await {
		&services.globals.server_user
	} else {
		sender_user
	};

	let (replacement_room, state_lock) = match version_rules.room_id_format {
		| RoomIdFormatVersion::V2 =>
			upgrade_room_create(
				&services,
				creator,
				old_room_id,
				new_version,
				&version_rules,
				predecessor,
				body.additional_creators.clone(),
			)
			.await,

		| RoomIdFormatVersion::V1 =>
			upgrade_room_create_legacy(
				&services,
				creator,
				old_room_id,
				new_version,
				&version_rules,
				predecessor,
			)
			.await,
	}
	.inspect_err(|e| error!(?body, "Upgrade m.room.create event failed: {e}"))?;

	let old_room_id = &body.room_id;
	let old_version = services
		.state
		.get_room_version(old_room_id)
		.await?;
	let old_version_rules = room_version::rules(&old_version)?;

	let context = RoomUpgradeContext {
		services: &services,
		sender_user,
		creator,
		old_room_id,
		old_state_lock: &old_state_lock,
		old_version_rules: &old_version_rules,
		new_room_id: &replacement_room,
		new_state_lock: &state_lock,
		new_version_rules: &version_rules,
		additional_creators: &body.additional_creators,
	};

	if let Err(e) = context.transfer_room().await {
		error!(?e, ?context, "Room upgrade failed. Cleaning up incomplete room...");

		if let Err(e) = services
			.delete
			.delete_room(&replacement_room, false, state_lock)
			.await
		{
			error!("Additional errors while deleting incomplete room: {e}");
		}

		return Err(e);
	}

	info!(
		old_room_id = %context.old_room_id,
		new_room_id = %context.new_room_id,
		upgraded_by = %sender_user,
		"Room upgraded",
	);

	Ok(v3::Response { replacement_room })
}

#[tracing::instrument(level = "info")]
async fn upgrade_room_create(
	services: &Services,
	sender_user: &UserId,
	old_room_id: &RoomId,
	new_version: &RoomVersionId,
	version_rules: &RoomVersionRules,
	predecessor: PreviousRoom,
	mut additional_creators: Vec<OwnedUserId>,
) -> Result<(OwnedRoomId, RoomMutexGuard)> {
	// Get the old room creation event
	let mut content: CanonicalJsonObject = services
		.state_accessor
		.room_state_get_content(old_room_id, &StateEventType::RoomCreate, "")
		.await
		.map_err(|_| err!(Database("Found room without m.room.create event.")))?;

	content.remove("creator");
	content.insert("predecessor".into(), json!(predecessor).try_into()?);
	content.insert("room_version".into(), json!(new_version).try_into()?);

	if version_rules
		.authorization
		.additional_room_creators
	{
		additional_creators.sort();
		additional_creators.dedup();
		content.remove("additional_creators");
		if !additional_creators.is_empty() {
			content.insert("additional_creators".into(), json!(additional_creators).try_into()?);
		}
	}

	// Validate creation event content
	let raw_content = to_raw_value(&content)?;
	if let Err(e) = serde_json::from_str::<CanonicalJsonObject>(raw_content.get()) {
		return Err!(Request(BadJson("Error forming creation event: {e}")));
	}

	let room_id = ruma::room_id!("!thiswillbereplaced").to_owned();
	let state_lock = services.state.mutex.lock(&room_id).await;
	let create_event_id = services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomCreate,
				content: to_raw_value(&content)?,
				state_key: Some(StateKey::new()),
				..Default::default()
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.boxed()
		.await?;

	drop(state_lock);

	// The real room_id is now the event_id.
	let room_id = OwnedRoomId::from_parts('!', create_event_id.localpart(), None)?;
	let state_lock = services.state.mutex.lock(&room_id).await;

	Ok((room_id, state_lock))
}

#[tracing::instrument(level = "info")]
async fn upgrade_room_create_legacy(
	services: &Services,
	sender_user: &UserId,
	old_room_id: &RoomId,
	new_version: &RoomVersionId,
	version_rules: &RoomVersionRules,
	predecessor: PreviousRoom,
) -> Result<(OwnedRoomId, RoomMutexGuard)> {
	// Create a replacement room
	let new_room_id = RoomId::new_v1(services.globals.server_name());
	let state_lock = services.state.mutex.lock(&new_room_id).await;
	let _short_id = services
		.short
		.get_or_create_shortroomid(&new_room_id)
		.await;

	// Get the old room creation event
	let mut content: CanonicalJsonObject = services
		.state_accessor
		.room_state_get_content(old_room_id, &StateEventType::RoomCreate, "")
		.await
		.map_err(|_| err!(Database("Found room without m.room.create event.")))?;

	// Send a m.room.create event containing a predecessor field and the applicable
	// room_version. "creator" key no longer exists in V11+ rooms.
	{
		use RoomVersionId::*;
		match new_version {
			| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 =>
				content.insert("creator".into(), json!(&sender_user).try_into()?),
			| _ => content.remove("creator"),
		}
	};

	content.insert("predecessor".into(), json!(predecessor).try_into()?);
	content.insert("room_version".into(), json!(new_version).try_into()?);

	// Validate creation event content
	let raw_content = to_raw_value(&content)?;
	if let Err(e) = serde_json::from_str::<CanonicalJsonObject>(raw_content.get()) {
		return Err!(Request(BadJson("Error forming creation event: {e}")));
	}

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomCreate,
				content: to_raw_value(&content)?,
				state_key: Some(StateKey::new()),
				..Default::default()
			},
			sender_user,
			&new_room_id,
			&state_lock,
		)
		.await?;

	Ok((new_room_id, state_lock))
}

#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn transfer_room(&self) -> Result {
	self.move_creator().await?;

	self.move_state_events().await?;

	self.move_sender_user().await?;

	self.move_local_aliases().await?;

	self.tombstone_old_room().await?;

	// After commitment to the tombstone above no more errors can propagate.
	self.lockdown_old_room()
		.await
		.inspect_err(|e| error!(?self, "Failed to lockdown old room: {e}"))
		.ok();

	Ok(())
}

// Join the new room
#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn move_creator(&self) -> Result {
	self.move_member(self.creator).await?;

	Ok(())
}

#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn move_sender_user(&self) -> Result {
	if self.sender_user != self.creator {
		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder::state(
					self.sender_user.as_str(),
					&RoomMemberEventContent::new(MembershipState::Invite),
				),
				self.creator,
				self.new_room_id,
				self.new_state_lock,
			)
			.await?;

		self.move_member(self.sender_user).await?;
	}

	Ok(())
}

#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn move_member(&self, user_id: &UserId) -> Result {
	let old_content: RoomMemberEventContent = self
		.services
		.state_accessor
		.room_state_get_content(self.old_room_id, &StateEventType::RoomMember, user_id.as_str())
		.inspect_err(|e| error!(?self, "Missing room member event: {e}"))
		.await?;

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(user_id.as_str(), &RoomMemberEventContent {
				membership: MembershipState::Join,
				join_authorized_via_users_server: None,
				..old_content
			}),
			user_id,
			self.new_room_id,
			self.new_state_lock,
		)
		.await?;

	Ok(())
}

// Replicate transferable state events to the new room
#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn move_state_events(&self) -> Result {
	RECOMMENDED_TRANSFERABLE_STATE_EVENT_TYPES
		.iter()
		.rev()
		.stream()
		.wide_filter_map(|event_type| {
			self.services
				.state_accessor
				.room_state_get(self.old_room_id, event_type, "")
				.ok()
		})
		.map(Ok)
		.try_for_each(async |event| {
			self.services
				.timeline
				.build_and_append_pdu(
					self.rebuild_state_event(&event).await?,
					self.creator,
					self.new_room_id,
					self.new_state_lock,
				)
				.inspect_err(|e| {
					error!(?event, ?self, "Failed to transfer state on upgrade: {e}");
				})
				.map_ok(|_| ())
				.await
		})
		.await
}

#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn rebuild_state_event<Pdu: Event>(&self, event: &Pdu) -> Result<PduBuilder> {
	let content = match event.kind() {
		| TimelineEventType::RoomPowerLevels => {
			let mut content = event.get_content_as_value();

			if self
				.new_version_rules
				.authorization
				.explicitly_privilege_room_creators
			{
				if let Some(users) = content
					.get_mut("users")
					.and_then(JsonValue::as_object_mut)
				{
					users.retain(|user_id, _pl| {
						!self
							.additional_creators
							.iter()
							.map(AsRef::as_ref)
							.chain(once(self.creator))
							.map(UserId::as_str)
							.any(is_equal_to!(user_id.as_str()))
					});
				}

				if self.creator == self.sender_user
					&& content["events"]["m.room.tombstone"]
						.as_i64()
						.is_none_or(is_less_than!(150))
				{
					content["events"]["m.room.tombstone"] = json!(150);
				}
			} else if self
				.old_version_rules
				.authorization
				.explicitly_privilege_room_creators
			{
				#[expect(clippy::collapsible_if)]
				if let Some(users) = content
					.as_object_mut()
					.expect("power levels event content must be an object")
					.entry("users")
					.or_insert(json!({}))
					.as_object_mut()
				{
					let level = json!(1000);

					self.services
						.state_accessor
						.get_create(self.old_room_id)
						.await?
						.creators(&self.old_version_rules.authorization)?
						.for_each(|user_id| {
							users.insert(user_id.to_string(), level.clone());
						});
				}
			}

			to_raw_value(&content)?
		},
		| _ => to_raw_value(event.content())?,
	};

	Ok(PduBuilder {
		content,
		event_type: event.kind().clone(),
		state_key: event.state_key().map(Into::into),
		..Default::default()
	})
}

// Moves any local aliases to the new room
#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn move_local_aliases(&self) -> Result {
	self.services
		.alias
		.local_aliases_for_room(self.old_room_id)
		.ready_for_each(|alias| {
			self.services
				.alias
				.set_alias_by(alias, self.new_room_id, self.creator)
				.inspect_err(|e| error!(?self, "Failed to add alias: {e}"))
				.ok();
		})
		.map(Ok)
		.await
}

// Send a m.room.tombstone event to the old room to indicate that it is not
// intended to be used any further Fail if the sender does not have the required
// permissions.
#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn tombstone_old_room(&self) -> Result<OwnedEventId> {
	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(StateKey::new(), &RoomTombstoneEventContent {
				body: "This room has been upgraded.".to_owned(),
				replacement_room: self.new_room_id.to_owned(),
			}),
			self.sender_user,
			self.old_room_id,
			self.old_state_lock,
		)
		.await
}

// Modify the power levels in the old room to prevent sending of events and
// inviting new users. Though a Result is returned, the callsite above treats it
// as infallible because the tombstone represents the commitment.
#[implement(RoomUpgradeContext, params = "<'_>")]
#[tracing::instrument(level = "debug")]
async fn lockdown_old_room(&self) -> Result<OwnedEventId> {
	// Get the old room power levels
	let old_content: RoomPowerLevelsEventContent = self
		.services
		.state_accessor
		.room_state_get_content(self.old_room_id, &StateEventType::RoomPowerLevels, "")
		.await
		.map_err(|_| err!(Database("Found room without m.room.power_levels event.")))?;

	let old_users_default = old_content
		.users_default
		.checked_add(int!(1))
		.ok_or_else(|| {
			err!(Request(BadJson("users_default power levels event content is not valid")))
		})?;

	// Setting events_default and invite to the greater of 50 and users_default + 1
	let new_level = max(int!(50), old_users_default);

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(StateKey::new(), &RoomPowerLevelsEventContent {
				events_default: new_level,
				invite: new_level,
				..old_content
			}),
			self.sender_user,
			self.old_room_id,
			self.old_state_lock,
		)
		.await
}
