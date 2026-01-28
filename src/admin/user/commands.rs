use std::{cmp, collections::BTreeMap};

use futures::{FutureExt, StreamExt, TryStreamExt};
use ruma::{
	Int, OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedUserId, UserId,
	events::{
		RoomAccountDataEventType, StateEventType,
		room::{
			power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent, UserPowerLevel},
			redaction::RoomRedactionEventContent,
		},
		tag::{TagEvent, TagEventContent, TagInfo},
	},
	uint,
};
use tuwunel_core::{
	Err, Result, debug_warn, info,
	matrix::{Event, pdu::PduBuilder},
	utils::{self, ReadyExt, stream::IterStream},
};
use tuwunel_service::{Services, users::Register};

use crate::{
	admin_command, get_room_info,
	utils::{parse_active_local_user_id, parse_local_user_id, parse_user_id},
};

const AUTO_GEN_PASSWORD_LENGTH: usize = 25;
const BULK_JOIN_REASON: &str = "Bulk force joining this room as initiated by the server admin.";

#[admin_command]
pub(super) async fn list_users(&self) -> Result {
	let users: Vec<_> = self
		.services
		.users
		.list_local_users()
		.map(ToString::to_string)
		.collect()
		.await;

	let mut plain_msg = format!("Found {} local user account(s):\n```\n", users.len());
	plain_msg += users.join("\n").as_str();
	plain_msg += "\n```";

	self.write_str(&plain_msg).await
}

#[admin_command]
pub(super) async fn create_user(&self, username: String, password: Option<String>) -> Result {
	// Validate user id
	let user_id = parse_local_user_id(self.services, &username)?;

	if let Err(e) = user_id.validate_strict()
		&& self.services.config.emergency_password.is_none()
	{
		return Err!("Username {user_id} contains disallowed characters or spaces: {e}");
	}

	if self.services.users.exists(&user_id).await {
		return Err!("User {user_id} already exists");
	}

	let password = password.unwrap_or_else(|| utils::random_string(AUTO_GEN_PASSWORD_LENGTH));

	self.services
		.users
		.full_register(Register {
			user_id: Some(&user_id),
			password: Some(&password),
			grant_first_user_admin: true,
			..Default::default()
		})
		.await?;

	self.write_str(&format!("Created user with user_id: {user_id} and password: `{password}`"))
		.await
}

#[admin_command]
pub(super) async fn deactivate(&self, no_leave_rooms: bool, user_id: String) -> Result {
	// Validate user id
	let user_id = parse_local_user_id(self.services, &user_id)?;

	// don't deactivate the server service account
	if user_id == self.services.globals.server_user {
		return Err!("Not allowed to deactivate the server service account.",);
	}

	deactivate_user(self.services, &user_id, no_leave_rooms).await?;

	self.write_str(&format!("User {user_id} has been deactivated"))
		.await
}

#[admin_command]
pub(super) async fn delete_device(
	&self,
	user_id: OwnedUserId,
	device_id: OwnedDeviceId,
) -> Result {
	if !self.services.globals.user_is_local(&user_id) {
		return Err!("Cannot delete device of remote user");
	}

	self.services
		.users
		.remove_device(&user_id, &device_id)
		.await;

	self.write_str(&format!("User {user_id}'s device {device_id} removed."))
		.await
}

#[admin_command]
pub(super) async fn reset_password(&self, username: String, password: Option<String>) -> Result {
	let user_id = parse_local_user_id(self.services, &username)?;

	if user_id == self.services.globals.server_user {
		return Err!(
			"Not allowed to set the password for the server account. Please use the emergency \
			 password config option.",
		);
	}

	let new_password = password.unwrap_or_else(|| utils::random_string(AUTO_GEN_PASSWORD_LENGTH));

	match self
		.services
		.users
		.set_password(&user_id, Some(new_password.as_str()))
		.await
	{
		| Err(e) => return Err!("Couldn't reset the password for user {user_id}: {e}"),
		| Ok(()) => {
			write!(self, "Successfully reset the password for user {user_id}: `{new_password}`")
		},
	}
	.await
}

#[admin_command]
pub(super) async fn deactivate_all(&self, no_leave_rooms: bool, force: bool) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	let usernames = self
		.body
		.to_vec()
		.drain(1..self.body.len().saturating_sub(1))
		.collect::<Vec<_>>();

	let mut user_ids: Vec<OwnedUserId> = Vec::with_capacity(usernames.len());
	let mut admins = Vec::new();

	for username in usernames {
		match parse_active_local_user_id(self.services, username).await {
			| Err(e) => {
				self.services
					.admin
					.send_text(&format!("{username} is not a valid username, skipping over: {e}"))
					.await;

				continue;
			},
			| Ok(user_id) => {
				if self.services.users.is_admin(&user_id).await && !force {
					self.services
						.admin
						.send_text(&format!(
							"{username} is an admin and --force is not set, skipping over"
						))
						.await;

					admins.push(username);
					continue;
				}

				// don't deactivate the server service account
				if user_id == self.services.globals.server_user {
					self.services
						.admin
						.send_text(&format!(
							"{username} is the server service account, skipping over"
						))
						.await;

					continue;
				}

				user_ids.push(user_id);
			},
		}
	}

	let mut deactivation_count: usize = 0;

	for user_id in user_ids {
		match deactivate_user(self.services, &user_id, no_leave_rooms).await {
			| Ok(()) => {
				deactivation_count = deactivation_count.saturating_add(1);
			},
			| Err(e) => {
				self.services
					.admin
					.send_text(&format!("Failed deactivating user: {e}"))
					.await;
			},
		}
	}

	if admins.is_empty() {
		write!(self, "Deactivated {deactivation_count} accounts.")
	} else {
		write!(
			self,
			"Deactivated {deactivation_count} accounts.\nSkipped admin accounts: {}. Use \
			 --force to deactivate admin accounts",
			admins.join(", ")
		)
	}
	.await
}

async fn deactivate_user(services: &Services, user_id: &UserId, no_leave_rooms: bool) -> Result {
	if !no_leave_rooms {
		services
			.deactivate
			.full_deactivate(user_id)
			.boxed()
			.await?;
	} else {
		services.users.deactivate_account(user_id).await?;
	}

	Ok(())
}

#[admin_command]
pub(super) async fn list_joined_rooms(&self, user_id: String) -> Result {
	// Validate user id
	let user_id = parse_local_user_id(self.services, &user_id)?;

	let mut rooms: Vec<(OwnedRoomId, u64, String)> = self
		.services
		.state_cache
		.rooms_joined(&user_id)
		.then(|room_id| get_room_info(self.services, room_id))
		.collect()
		.await;

	if rooms.is_empty() {
		return Err!("User is not in any rooms.");
	}

	rooms.sort_by_key(|r| r.1);
	rooms.reverse();

	let body = rooms
		.iter()
		.map(|(id, members, name)| format!("{id}\tMembers: {members}\tName: {name}"))
		.collect::<Vec<_>>()
		.join("\n");

	self.write_str(&format!("Rooms {user_id} Joined ({}):\n```\n{body}\n```", rooms.len(),))
		.await
}

#[admin_command]
pub(super) async fn force_join_list_of_local_users(
	&self,
	room: OwnedRoomOrAliasId,
	yes_i_want_to_do_this: bool,
) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	if !yes_i_want_to_do_this {
		return Err!(
			"You must pass the --yes-i-want-to-do-this flag to ensure you really want to force \
			 bulk join all specified local users.",
		);
	}

	let Ok(admin_room) = self.services.admin.get_admin_room().await else {
		return Err!("There is not an admin room to check for server admins.",);
	};

	let (room_id, servers) = self
		.services
		.alias
		.maybe_resolve_with_servers(&room, None)
		.await?;

	if !self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), &room_id)
		.await
	{
		return Err!("We are not joined in this room.");
	}

	let server_admins: Vec<_> = self
		.services
		.state_cache
		.active_local_users_in_room(&admin_room)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	if !self
		.services
		.state_cache
		.room_members(&room_id)
		.ready_any(|user_id| server_admins.contains(&user_id.to_owned()))
		.await
	{
		return Err!("There is not a single server admin in the room.",);
	}

	let usernames = self
		.body
		.to_vec()
		.drain(1..self.body.len().saturating_sub(1))
		.collect::<Vec<_>>();

	let mut user_ids: Vec<OwnedUserId> = Vec::with_capacity(usernames.len());

	for username in usernames {
		match parse_active_local_user_id(self.services, username).await {
			| Ok(user_id) => {
				// don't make the server service account join
				if user_id == self.services.globals.server_user {
					self.services
						.admin
						.send_text(&format!(
							"{username} is the server service account, skipping over"
						))
						.await;

					continue;
				}

				user_ids.push(user_id);
			},
			| Err(e) => {
				self.services
					.admin
					.send_text(&format!("{username} is not a valid username, skipping over: {e}"))
					.await;

				continue;
			},
		}
	}

	let mut failed_joins: usize = 0;
	let mut successful_joins: usize = 0;

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	for user_id in user_ids {
		match self
			.services
			.membership
			.join(
				&user_id,
				&room_id,
				Some(&room),
				Some(String::from(BULK_JOIN_REASON)),
				&servers,
				false,
				&state_lock,
			)
			.await
		{
			| Ok(_res) => {
				successful_joins = successful_joins.saturating_add(1);
			},
			| Err(e) => {
				debug_warn!("Failed force joining {user_id} to {room_id} during bulk join: {e}");
				failed_joins = failed_joins.saturating_add(1);
			},
		}
	}

	drop(state_lock);

	self.write_str(&format!(
		"{successful_joins} local users have been joined to {room_id}. {failed_joins} joins \
		 failed.",
	))
	.await
}

#[admin_command]
pub(super) async fn force_join_all_local_users(
	&self,
	room: OwnedRoomOrAliasId,
	yes_i_want_to_do_this: bool,
) -> Result {
	if !yes_i_want_to_do_this {
		return Err!(
			"You must pass the --yes-i-want-to-do-this-flag to ensure you really want to force \
			 bulk join all local users.",
		);
	}

	let Ok(admin_room) = self.services.admin.get_admin_room().await else {
		return Err!("There is not an admin room to check for server admins.",);
	};

	let (room_id, servers) = self
		.services
		.alias
		.maybe_resolve_with_servers(&room, None)
		.await?;

	if !self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), &room_id)
		.await
	{
		return Err!("We are not joined in this room.");
	}

	let server_admins: Vec<_> = self
		.services
		.state_cache
		.active_local_users_in_room(&admin_room)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	if !self
		.services
		.state_cache
		.room_members(&room_id)
		.ready_any(|user_id| server_admins.contains(&user_id.to_owned()))
		.await
	{
		return Err!("There is not a single server admin in the room.",);
	}

	let mut failed_joins: usize = 0;
	let mut successful_joins: usize = 0;

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	for user_id in &self
		.services
		.users
		.list_local_users()
		.map(UserId::to_owned)
		.collect::<Vec<_>>()
		.await
	{
		match self
			.services
			.membership
			.join(
				user_id,
				&room_id,
				Some(&room),
				Some(String::from(BULK_JOIN_REASON)),
				&servers,
				false,
				&state_lock,
			)
			.await
		{
			| Ok(_res) => {
				successful_joins = successful_joins.saturating_add(1);
			},
			| Err(e) => {
				debug_warn!("Failed force joining {user_id} to {room_id} during bulk join: {e}");
				failed_joins = failed_joins.saturating_add(1);
			},
		}
	}

	drop(state_lock);

	self.write_str(&format!(
		"{successful_joins} local users have been joined to {room_id}. {failed_joins} joins \
		 failed.",
	))
	.await
}

#[admin_command]
pub(super) async fn force_join_room(&self, user_id: String, room: OwnedRoomOrAliasId) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;
	let (room_id, servers) = self
		.services
		.alias
		.maybe_resolve_with_servers(&room, None)
		.await?;

	assert!(
		self.services.globals.user_is_local(&user_id),
		"Parsed user_id must be a local user"
	);

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	self.services
		.membership
		.join(&user_id, &room_id, Some(&room), None, &servers, false, &state_lock)
		.await?;

	drop(state_lock);

	self.write_str(&format!("{user_id} has been joined to {room_id}.",))
		.await
}

#[admin_command]
pub(super) async fn force_leave_room(
	&self,
	user_id: String,
	room_id: OwnedRoomOrAliasId,
) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	assert!(
		self.services.globals.user_is_local(&user_id),
		"Parsed user_id must be a local user"
	);

	if !self
		.services
		.state_cache
		.is_joined(&user_id, &room_id)
		.await
	{
		return Err!("{user_id} is not joined in the room");
	}

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	self.services
		.membership
		.leave(&user_id, &room_id, None, false, &state_lock)
		.boxed()
		.await?;

	drop(state_lock);

	self.write_str(&format!("{user_id} has left {room_id}.",))
		.await
}

#[admin_command]
pub(super) async fn force_demote(&self, user_id: String, room_id: OwnedRoomOrAliasId) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	assert!(
		self.services.globals.user_is_local(&user_id),
		"Parsed user_id must be a local user"
	);

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	let room_power_levels: Option<RoomPowerLevels> = self
		.services
		.state_accessor
		.get_power_levels(&room_id)
		.await
		.ok();

	let user_can_change_self = room_power_levels
		.as_ref()
		.is_some_and(|power_levels| {
			power_levels.user_can_change_user_power_level(&user_id, &user_id)
		});

	let user_can_demote_self = user_can_change_self
		|| self
			.services
			.state_accessor
			.room_state_get(&room_id, &StateEventType::RoomCreate, "")
			.await
			.is_ok_and(|event| event.sender() == user_id);

	if !user_can_demote_self {
		return Err!("User is not allowed to modify their own power levels in the room.");
	}

	let mut power_levels_content: RoomPowerLevelsEventContent = room_power_levels
		.map(TryInto::try_into)
		.transpose()?
		.unwrap_or_default();

	power_levels_content.users.remove(&user_id);

	let event_id = self
		.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &power_levels_content),
			&user_id,
			&room_id,
			&state_lock,
		)
		.await?;

	self.write_str(&format!(
		"User {user_id} demoted themselves to the room default power level in {room_id} - \
		 {event_id}"
	))
	.await
}

#[admin_command]
pub(super) async fn force_promote(
	&self,
	target_id: String,
	room_id: OwnedRoomOrAliasId,
) -> Result {
	let target_id = parse_user_id(self.services, &target_id)?;
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	let room_power_levels = self
		.services
		.state_accessor
		.get_power_levels(&room_id)
		.await?;

	let privileged_member = self
		.services
		.state_cache
		.room_members(&room_id)
		.ready_filter(|member_id| {
			self.services.globals.user_is_local(member_id)
				&& room_power_levels.user_can_change_user_power_level(member_id, &target_id)
		})
		.map(ToOwned::to_owned)
		.ready_fold_default(|selected_user, member_id| match selected_user {
			| None => Some(member_id),
			| Some(selected_user) => Some(
				if room_power_levels.for_user(&selected_user)
					> room_power_levels.for_user(&member_id)
				{
					selected_user
				} else {
					member_id
				},
			),
		})
		.await;

	let Some(privileged_member) = privileged_member else {
		return Err!("No privileged user exists in room, cannot promote.");
	};

	info!("Selected privileged member {privileged_member}");

	let power_level: Int = match room_power_levels.for_user(&privileged_member) {
		| UserPowerLevel::Infinite => Int::MAX,
		| UserPowerLevel::Int(x) => x,
	};

	let mut power_levels_content: RoomPowerLevelsEventContent = room_power_levels.try_into()?;

	power_levels_content
		.users
		.insert(target_id.clone(), power_level);

	let event_id = self
		.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(String::new(), &power_levels_content),
			&privileged_member,
			&room_id,
			&state_lock,
		)
		.await?;

	drop(state_lock);

	self.write_str(&format!(
		"User {privileged_member} promoted {target_id} to {power_level} power level in \
		 {room_id} - {event_id}"
	))
	.await?;

	Ok(())
}

#[admin_command]
pub(super) async fn make_user_admin(&self, user_id: String) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;
	assert!(
		self.services.globals.user_is_local(&user_id),
		"Parsed user_id must be a local user"
	);

	self.services
		.admin
		.make_user_admin(&user_id)
		.boxed()
		.await?;

	self.write_str(&format!("{user_id} has been granted admin privileges.",))
		.await
}

#[admin_command]
pub(super) async fn put_room_tag(
	&self,
	user_id: String,
	room_id: OwnedRoomId,
	tag: String,
) -> Result {
	let user_id = parse_active_local_user_id(self.services, &user_id).await?;

	let mut tags_event = self
		.services
		.account_data
		.get_room(&room_id, &user_id, RoomAccountDataEventType::Tag)
		.await
		.unwrap_or(TagEvent {
			content: TagEventContent { tags: BTreeMap::new() },
		});

	tags_event
		.content
		.tags
		.insert(tag.clone().into(), TagInfo::new());

	self.services
		.account_data
		.update(
			Some(&room_id),
			&user_id,
			RoomAccountDataEventType::Tag,
			&serde_json::to_value(tags_event).expect("to json value always works"),
		)
		.await?;

	self.write_str(&format!(
		"Successfully updated room account data for {user_id} and room {room_id} with tag {tag}"
	))
	.await
}

#[admin_command]
pub(super) async fn delete_room_tag(
	&self,
	user_id: String,
	room_id: OwnedRoomId,
	tag: String,
) -> Result {
	let user_id = parse_active_local_user_id(self.services, &user_id).await?;

	let mut tags_event = self
		.services
		.account_data
		.get_room(&room_id, &user_id, RoomAccountDataEventType::Tag)
		.await
		.unwrap_or(TagEvent {
			content: TagEventContent { tags: BTreeMap::new() },
		});

	tags_event
		.content
		.tags
		.remove(&tag.clone().into());

	self.services
		.account_data
		.update(
			Some(&room_id),
			&user_id,
			RoomAccountDataEventType::Tag,
			&serde_json::to_value(tags_event).expect("to json value always works"),
		)
		.await?;

	self.write_str(&format!(
		"Successfully updated room account data for {user_id} and room {room_id}, deleting room \
		 tag {tag}"
	))
	.await
}

#[admin_command]
pub(super) async fn get_room_tags(&self, user_id: String, room_id: OwnedRoomId) -> Result {
	let user_id = parse_active_local_user_id(self.services, &user_id).await?;

	let tags_event = self
		.services
		.account_data
		.get_room(&room_id, &user_id, RoomAccountDataEventType::Tag)
		.await
		.unwrap_or(TagEvent {
			content: TagEventContent { tags: BTreeMap::new() },
		});

	self.write_str(&format!("```\n{:#?}\n```", tags_event.content.tags))
		.await
}

#[admin_command]
pub(super) async fn redact_event(&self, event_id: OwnedEventId) -> Result {
	let Ok(event) = self
		.services
		.timeline
		.get_non_outlier_pdu(&event_id)
		.await
	else {
		return Err!("Event does not exist in our database.");
	};

	if event.is_redacted() {
		return Err!("Event is already redacted.");
	}

	if !self
		.services
		.globals
		.user_is_local(event.sender())
	{
		return Err!("This command only works on local users.");
	}

	let reason = format!(
		"The administrator(s) of {} has redacted this user's message.",
		self.services.globals.server_name()
	);

	let redaction_event_id = {
		let state_lock = self
			.services
			.state
			.mutex
			.lock(event.room_id())
			.await;

		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					redacts: Some(event.event_id().to_owned()),
					..PduBuilder::timeline(&RoomRedactionEventContent {
						redacts: Some(event.event_id().to_owned()),
						reason: Some(reason),
					})
				},
				event.sender(),
				event.room_id(),
				&state_lock,
			)
			.await?
	};

	self.write_str(&format!(
		"Successfully redacted event. Redaction event ID: {redaction_event_id}"
	))
	.await
}

#[admin_command]
pub(super) async fn last_active(&self, limit: Option<usize>) -> Result {
	self.services
		.users
		.list_local_users()
		.map(ToOwned::to_owned)
		.then(async |user_id| {
			self.services
				.users
				.all_devices_metadata(&user_id)
				.ready_filter_map(|device| device.last_seen_ts)
				.ready_fold_default(cmp::max)
				.map(|last_seen_ts| (last_seen_ts, user_id.clone()))
				.await
		})
		.ready_filter(|(ts, _)| ts.get() > uint!(0))
		.collect::<Vec<_>>()
		.map(|mut vec| {
			vec.sort_by_key(|k| cmp::Reverse(k.0));
			vec
		})
		.map(Vec::into_iter)
		.map(IterStream::try_stream)
		.flatten_stream()
		.take(limit.unwrap_or(48))
		.try_for_each(async |(last_seen_ts, user_id)| {
			let ago = last_seen_ts;
			let user_id = user_id.localpart();
			let line = format!("{ago:?} {user_id}\n");

			self.write_str(&line).await
		})
		.boxed()
		.await
}
