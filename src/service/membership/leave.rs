use std::collections::HashSet;

use futures::{FutureExt, StreamExt, TryFutureExt, future::ready, pin_mut};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedServerName, RoomId, UserId,
	api::federation,
	canonical_json::to_canonical_value,
	events::{
		AnyStrippedStateEvent, StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	serde::Raw,
};
use tuwunel_core::{
	Err, Error, Result, async_noinline, debug_info, debug_warn, err, implement,
	matrix::{PduCount, pdu::check_rules, room_version},
	pdu::PduBuilder,
	utils::{self, FutureBoolExt, future::ReadyBoolExt},
	warn,
};

use super::Service;
use crate::rooms::timeline::RoomMutexGuard;

#[implement(Service)]
#[async_noinline]
#[tracing::instrument(
    name = "leave",
    level = "debug",
    skip_all,
    fields(%room_id, %user_id)
)]
pub async fn leave<'a>(
	&'a self,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	reason: Option<String>,
	remote_leave_now: bool,
	state_lock: &'a RoomMutexGuard,
) -> Result {
	let leave_content = RoomMemberEventContent {
		membership: MembershipState::Leave,
		reason: reason.clone(),
		join_authorized_via_users_server: None,
		is_direct: false,
		avatar_url: None,
		displayname: None,
		third_party_invite: None,
		blurhash: None,
	};

	let is_banned = self.services.metadata.is_banned(room_id);
	let is_disabled = self.services.metadata.is_disabled(room_id);
	pin_mut!(is_banned, is_disabled);
	if is_banned.or(is_disabled).await {
		return self
			.clear_local_leave(user_id, room_id, leave_content, None)
			.await;
	}

	let member_event = self
		.services
		.state_accessor
		.room_state_get_content::<RoomMemberEventContent>(
			room_id,
			&StateEventType::RoomMember,
			user_id.as_str(),
		)
		.await;

	let dont_have_room = self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), room_id)
		.is_false()
		.and(ready(member_event.as_ref().is_err()));

	let not_knocked = self
		.services
		.state_cache
		.is_knocked(user_id, room_id)
		.is_false();

	if remote_leave_now || dont_have_room.and(not_knocked).await {
		self.leave_via_remote(user_id, room_id, reason, leave_content)
			.await
	} else {
		self.leave_locally(user_id, room_id, reason, leave_content, member_event, state_lock)
			.await
	}
}

#[implement(Service)]
async fn leave_via_remote(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	leave_content: RoomMemberEventContent,
) -> Result {
	if let Err(e) = self
		.remote_leave(user_id, room_id, reason)
		.boxed()
		.await
	{
		warn!(%user_id, "Failed to leave room {room_id} remotely: {e}");
	}

	let last_state = self
		.last_known_strip_state(user_id, room_id)
		.await;

	self.clear_local_leave(user_id, room_id, leave_content, last_state)
		.await
}

#[implement(Service)]
async fn last_known_strip_state(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
) -> Option<Vec<Raw<AnyStrippedStateEvent>>> {
	self.services
		.state_cache
		.invite_state(user_id, room_id)
		.or_else(|_| {
			self.services
				.state_cache
				.knock_state(user_id, room_id)
		})
		.or_else(|_| {
			self.services
				.state_cache
				.left_state(user_id, room_id)
		})
		.await
		.ok()
}

#[implement(Service)]
async fn leave_locally(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	leave_content: RoomMemberEventContent,
	member_event: Result<RoomMemberEventContent>,
	state_lock: &RoomMutexGuard,
) -> Result {
	let Ok(event) = member_event else {
		debug_warn!(
			"Trying to leave a room you are not a member of, marking room as left locally."
		);

		return self
			.clear_local_leave(user_id, room_id, leave_content, None)
			.await;
	};

	if !is_leaveable(&event.membership) {
		debug_warn!(
			current = ?event.membership,
			"Room state shows non-leaveable membership; clearing local caches.",
		);

		return self
			.clear_local_leave(user_id, room_id, leave_content, None)
			.await;
	}

	let build_result = self
		.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(user_id.to_string(), &RoomMemberEventContent {
				membership: MembershipState::Leave,
				reason,
				join_authorized_via_users_server: None,
				is_direct: false,
				..event
			}),
			user_id,
			room_id,
			state_lock,
		)
		.await;

	// On state-res auth-check rejection, re-read membership. The pre-check above
	// and the auth_check inside build_and_append_pdu both run under state_lock,
	// so they observe the same state; re-reading here narrows the swallow to
	// non-leaveable membership (Leave/Ban/_Custom), which is the stale-state
	// population this branch targets. Genuine auth_check rejections against
	// fresh Invite/Join/Knock state propagate unchanged.
	match build_result {
		| Ok(_) => Ok(()),
		| Err(Error::AuthCheck(inner)) => {
			let current = self
				.services
				.state_accessor
				.room_state_get_content::<RoomMemberEventContent>(
					room_id,
					&StateEventType::RoomMember,
					user_id.as_str(),
				)
				.await
				.map(|c| c.membership);

			if current.as_ref().is_ok_and(is_leaveable) {
				return Err(Error::AuthCheck(inner));
			}

			warn!(
				error = %inner,
				?current,
				"Auth refused self-leave PDU; clearing local caches.",
			);

			self.clear_local_leave(user_id, room_id, leave_content, None)
				.await
		},
		| Err(e) => Err(e),
	}
}

#[implement(Service)]
async fn clear_local_leave(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	leave_content: RoomMemberEventContent,
	last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
) -> Result {
	let count = self.services.globals.next_count();
	self.services
		.state_cache
		.update_membership(
			room_id,
			user_id,
			leave_content,
			user_id,
			last_state,
			None,
			true,
			PduCount::Normal(*count),
		)
		.await
}

#[implement(Service)]
#[tracing::instrument(name = "remote", level = "debug", skip_all)]
async fn remote_leave(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
) -> Result {
	let mut make_leave_response_and_server =
		Err!(BadServerResponse("No remote server available to assist in leaving {room_id}."));

	let mut servers: HashSet<OwnedServerName> = self
		.services
		.state_cache
		.servers_invite_via(room_id)
		.chain(self.services.state_cache.room_servers(room_id))
		.map(ToOwned::to_owned)
		.collect()
		.await;

	match self
		.services
		.state_cache
		.invite_state(user_id, room_id)
		.await
	{
		| Ok(invite_state) => {
			servers.extend(
				invite_state
					.iter()
					.filter_map(|event| event.get_field("sender").ok().flatten())
					.filter_map(|sender: &str| UserId::parse(sender).ok())
					.map(|user| user.server_name().to_owned()),
			);
		},
		| _ => {
			match self
				.services
				.state_cache
				.knock_state(user_id, room_id)
				.await
			{
				| Ok(knock_state) => {
					servers.extend(
						knock_state
							.iter()
							.filter_map(|event| event.get_field("sender").ok().flatten())
							.filter_map(|sender: &str| UserId::parse(sender).ok())
							.filter_map(|sender| {
								(!self.services.globals.user_is_local(&sender))
									.then(|| sender.server_name().to_owned())
							}),
					);
				},
				| _ => {},
			}
		},
	}

	servers.insert(user_id.server_name().to_owned());
	if let Some(room_id_server_name) = room_id.server_name() {
		servers.insert(room_id_server_name.to_owned());
	}

	debug_info!("servers in remote_leave_room: {servers:?}");

	for remote_server in servers
		.into_iter()
		.filter(|server| !self.services.globals.server_is_ours(server))
	{
		let make_leave_response = self
			.services
			.federation
			.execute(&remote_server, federation::membership::prepare_leave_event::v1::Request {
				room_id: room_id.to_owned(),
				user_id: user_id.to_owned(),
			})
			.await;

		make_leave_response_and_server = make_leave_response.map(|r| (r, remote_server));

		if make_leave_response_and_server.is_ok() {
			break;
		}
	}

	let (make_leave_response, remote_server) = make_leave_response_and_server?;

	let Some(room_version_id) = make_leave_response.room_version else {
		return Err!(BadServerResponse(warn!(
			"No room version was returned by {remote_server} for {room_id}, room version is \
			 likely not supported by tuwunel"
		)));
	};

	if !self
		.services
		.config
		.supported_room_version(&room_version_id)
	{
		return Err!(BadServerResponse(warn!(
			"Remote room version {room_version_id} for {room_id} is not supported by conduwuit",
		)));
	}

	let room_version_rules = room_version::rules(&room_version_id)?;

	let mut event = serde_json::from_str::<CanonicalJsonObject>(make_leave_response.event.get())
		.map_err(|e| {
			err!(BadServerResponse(warn!(
				"Invalid make_leave event json received from {remote_server} for {room_id}: \
				 {e:?}"
			)))
		})?;

	let mut content = RoomMemberEventContent {
		reason,
		..RoomMemberEventContent::new(MembershipState::Leave)
	};

	self.services
		.profile
		.fill_profile_data(user_id, &mut content)
		.await;

	event.insert("content".into(), to_canonical_value(content)?);

	event.insert(
		"origin".into(),
		CanonicalJsonValue::String(
			self.services
				.globals
				.server_name()
				.as_str()
				.to_owned(),
		),
	);

	event.insert(
		"origin_server_ts".into(),
		CanonicalJsonValue::Integer(utils::millis_since_unix_epoch().try_into()?),
	);

	event.insert("room_id".into(), CanonicalJsonValue::String(room_id.as_str().into()));

	event.insert("state_key".into(), CanonicalJsonValue::String(user_id.as_str().into()));

	event.insert("sender".into(), CanonicalJsonValue::String(user_id.as_str().into()));

	event.insert("type".into(), CanonicalJsonValue::String("m.room.member".into()));

	let event_id = self
		.services
		.server_keys
		.gen_id_hash_and_sign_event(&mut event, &room_version_id)?;

	check_rules(&event, &room_version_rules.event_format)?;

	self.services
		.federation
		.execute(&remote_server, federation::membership::create_leave_event::v2::Request {
			room_id: room_id.to_owned(),
			event_id,
			pdu: self
				.services
				.federation
				.format_pdu_into(event.clone(), Some(&room_version_id))
				.await,
		})
		.await?;

	Ok(())
}

/// Membership states permitted to transition to `Leave` via a self-leave PDU.
/// ruma's `MembershipState` is `#[non_exhaustive]`; future variants are
/// conservatively treated as non-leaveable.
fn is_leaveable(state: &MembershipState) -> bool {
	matches!(state, MembershipState::Invite | MembershipState::Join | MembershipState::Knock,)
}
