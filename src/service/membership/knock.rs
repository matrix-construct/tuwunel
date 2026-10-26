use std::{borrow::Borrow, iter::once};

use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedServerName, RoomId,
	RoomOrAliasId, RoomVersionId, UserId,
	api::federation::{
		self,
		membership::{
			RawStrippedState,
			create_knock_event::v1::{
				Request as SendKnockRequest, Response as SendKnockResponse,
			},
		},
	},
	canonical_json::to_canonical_value,
	events::{
		AnyStrippedStateEvent, StateEventType,
		receipt::ReceiptThread,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	room_version_rules::RoomVersionRules,
	serde::Raw,
	signatures::Verified,
};
use tuwunel_core::{
	Err, Result, async_noinline, at, debug_info, debug_warn, err, implement, info, trace,
	utils::{self, IterStream, result::LogErr, stream::WidebandExt},
	warn,
};
use tuwunel_matrix::{
	Event, PduCount,
	event::gen_event_id,
	pdu::{PduBuilder, PduEvent},
	room_version::rules as room_version_rules,
};

use super::{
	Service, StrippedCreateVerdict, dedup_stripped_state, enforce_stripped_create,
	stripped_state::occupies_cell, v12_room_ids, without_member,
};
use crate::{
	membership::join::get_servers_for_room,
	rooms::{state::RoomMutexGuard, state_cache::MembershipUpdate},
};

/// The stripped state a knock records as the room's summary for its user.
///
/// It is presentation data the user's client shows, never room state.
type Summary = Vec<Raw<AnyStrippedStateEvent>>;

#[implement(Service)]
#[async_noinline]
#[tracing::instrument(
	name = "knock",
	level = "debug",
	skip_all,
	fields(%sender_user, %room_id)
)]
pub async fn knock<'a>(
	&'a self,
	sender_user: &'a UserId,
	room_id: &'a RoomId,
	orig_server_name: Option<&'a RoomOrAliasId>,
	reason: Option<String>,
	servers: &'a [OwnedServerName],
	state_lock: &'a RoomMutexGuard,
) -> Result {
	let servers =
		get_servers_for_room(&self.services, sender_user, room_id, orig_server_name, servers)
			.await?;

	if self
		.services
		.state_cache
		.is_invited(sender_user, room_id)
		.await
	{
		debug_warn!(%sender_user, %room_id, "Invited user attempted to knock.");
		return Err!(Request(Forbidden(
			"You cannot knock on a room you are already invited/accepted to."
		)));
	}

	if self
		.services
		.state_cache
		.is_joined(sender_user, room_id)
		.await
	{
		debug_warn!(%sender_user, %room_id, "Joined user attempted to knock.");
		return Err!(Request(Forbidden("You cannot knock on a room you are already joined in.")));
	}

	let server_in_room = self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), room_id)
		.await;

	// Trust a local knock; re-drive a remote one in case we missed a kick.
	if server_in_room
		&& self
			.services
			.state_cache
			.is_knocked(sender_user, room_id)
			.await
	{
		debug_warn!(%sender_user, %room_id, "User is already knocking.");
		return Ok(());
	}

	if self
		.services
		.state_accessor
		.get_member(room_id, sender_user)
		.await
		.is_ok_and(|content| content.membership == MembershipState::Ban)
	{
		debug_warn!(%sender_user, %room_id, "Banned user attempted to knock.");
		return Err!(Request(Forbidden("You cannot knock on a room you are banned from.")));
	}

	let local_knock = server_in_room
		|| servers.is_empty()
		|| (servers.len() == 1 && self.services.globals.server_is_ours(&servers[0]));

	if local_knock {
		self.knock_room_helper_local(sender_user, room_id, reason, &servers, state_lock)
			.boxed()
			.await
	} else {
		self.knock_room_helper_remote(sender_user, room_id, reason, &servers)
			.boxed()
			.await
	}
}

#[implement(Service)]
async fn knock_room_helper_local(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	servers: &[OwnedServerName],
	state_lock: &RoomMutexGuard,
) -> Result {
	debug_info!("We can knock locally");

	let room_version_id = self
		.services
		.state
		.get_room_version(room_id)
		.await?;

	ensure_room_version_supports_knock(&room_version_id)?;

	let content = self
		.services
		.profile
		.fill_content(sender_user, RoomMemberEventContent {
			reason: reason.clone(),
			..RoomMemberEventContent::new(MembershipState::Knock)
		})
		.await;

	let Err(error) = self
		.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(sender_user.to_string(), &content),
			sender_user,
			room_id,
			state_lock,
		)
		.await
	else {
		return Ok(());
	};

	if servers.is_empty()
		|| (servers.len() == 1 && self.services.globals.server_is_ours(&servers[0]))
	{
		return Err(error);
	}

	warn!("We couldn't do the knock locally, maybe federation can help to satisfy the knock");

	self.knock_room_local_federation_fallback(sender_user, room_id, reason, servers, state_lock)
		.boxed()
		.await
}

fn ensure_room_version_supports_knock(room_version_id: &RoomVersionId) -> Result {
	if matches!(
		room_version_id,
		RoomVersionId::V1
			| RoomVersionId::V2
			| RoomVersionId::V3
			| RoomVersionId::V4
			| RoomVersionId::V5
			| RoomVersionId::V6
	) {
		return Err!(Request(Forbidden("This room does not support knocking.")));
	}

	Ok(())
}

#[implement(Service)]
async fn knock_room_local_federation_fallback(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	servers: &[OwnedServerName],
	state_lock: &RoomMutexGuard,
) -> Result {
	let (pdu, event) = self
		.knock_over_federation(sender_user, room_id, reason, servers)
		.await?;

	info!("Appending room knock event locally");
	self.services
		.timeline
		.append_pdu(&pdu, event, once(pdu.event_id.borrow()), state_lock)
		.await?;

	Ok(())
}

/// Knocks through a remote server on a room this server is not in.
///
/// Of the knock, only the user's membership and the room summary are stored.
/// None of the summary's events becomes an outlier, room state or a timeline
/// event, as the summary carries no auth chain to check them against.
#[implement(Service)]
async fn knock_room_helper_remote(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	servers: &[OwnedServerName],
) -> Result {
	info!("Knocking over federation.");

	self.knock_over_federation(sender_user, room_id, reason, servers)
		.await?;

	Ok(())
}

/// Knocks through a server in the room and records the user's knock.
///
/// Fails when this server does not support the remote room version. The
/// membership is recorded with the room summary from the accepting server's
/// response, as checked by [`Self::knock_summary`]. Returns the knock both
/// parsed and as the JSON this server signed.
#[implement(Service)]
async fn knock_over_federation(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	servers: &[OwnedServerName],
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let (make_knock_response, remote_server) = self
		.make_knock_request(sender_user, room_id, servers)
		.await?;

	info!("make_knock finished");

	let room_version_id = make_knock_response.room_version.clone();

	if !self
		.services
		.config
		.supported_room_version(&room_version_id)
	{
		return Err!(BadServerResponse(
			"Remote room version {room_version_id} is not supported by tuwunel"
		));
	}

	let (knock_event, event_id) = self
		.build_knock_event(sender_user, room_id, reason, &make_knock_response, &room_version_id)
		.await?;

	let send_knock_response = self
		.execute_send_knock(&remote_server, room_id, &event_id, &knock_event, &room_version_id)
		.await?;

	self.services
		.short
		.get_or_create_shortroomid(room_id)
		.await;

	info!("Parsing knock event");
	let pdu = PduEvent::from_object_and_eventid(&event_id, knock_event.clone())
		.map_err(|e| err!(BadServerResponse("Invalid knock event PDU: {e:?}")))?;

	let summary = self
		.knock_summary(room_id, &room_version_id, &pdu, send_knock_response.knock_room_state)
		.await?;

	self.record_knock(room_id, sender_user, &pdu, summary)
		.await?;

	Ok((pdu, knock_event))
}

#[implement(Service)]
async fn build_knock_event(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	reason: Option<String>,
	make_knock_response: &federation::membership::prepare_knock_event::v1::Response,
	room_version_id: &RoomVersionId,
) -> Result<(CanonicalJsonObject, OwnedEventId)> {
	let mut knock_event_stub: CanonicalJsonObject =
		serde_json::from_str(make_knock_response.event.get()).map_err(|e| {
			err!(BadServerResponse("Invalid make_knock event json received from server: {e:?}"))
		})?;

	let content = self
		.services
		.profile
		.fill_content(sender_user, RoomMemberEventContent {
			reason,
			..RoomMemberEventContent::new(MembershipState::Knock)
		})
		.await;

	knock_event_stub.insert(
		"origin".into(),
		CanonicalJsonValue::String(
			self.services
				.globals
				.server_name()
				.as_str()
				.to_owned(),
		),
	);
	knock_event_stub.insert(
		"origin_server_ts".into(),
		CanonicalJsonValue::Integer(
			utils::millis_since_unix_epoch()
				.try_into()
				.expect("Timestamp is valid js_int value"),
		),
	);
	knock_event_stub.insert(
		"content".into(),
		to_canonical_value(content).expect("event is valid, we just created it"),
	);

	knock_event_stub
		.insert("room_id".into(), CanonicalJsonValue::String(room_id.as_str().into()));

	knock_event_stub
		.insert("state_key".into(), CanonicalJsonValue::String(sender_user.as_str().into()));

	knock_event_stub
		.insert("sender".into(), CanonicalJsonValue::String(sender_user.as_str().into()));

	knock_event_stub.insert("type".into(), CanonicalJsonValue::String("m.room.member".into()));

	// In order to create a compatible ref hash (EventID) the `hashes` field needs
	// to be present
	self.services
		.server_keys
		.hash_and_sign_event(&mut knock_event_stub, room_version_id)?;

	let event_id = gen_event_id(&knock_event_stub, room_version_id)?;

	knock_event_stub
		.insert("event_id".into(), CanonicalJsonValue::String(event_id.clone().into()));

	Ok((knock_event_stub, event_id))
}

#[implement(Service)]
async fn execute_send_knock(
	&self,
	remote_server: &OwnedServerName,
	room_id: &RoomId,
	event_id: &OwnedEventId,
	knock_event: &CanonicalJsonObject,
	room_version_id: &RoomVersionId,
) -> Result<SendKnockResponse> {
	info!("Asking {remote_server} for send_knock in room {room_id}");
	let send_knock_request = SendKnockRequest {
		room_id: room_id.to_owned(),
		event_id: event_id.clone(),
		pdu: self
			.services
			.federation
			.format_pdu_into(knock_event.clone(), Some(room_version_id))
			.await,
	};

	let response = self
		.services
		.federation
		.execute(remote_server, send_knock_request)
		.await?;

	info!("send_knock finished");

	// Settled here so all three readers of this array agree.
	Ok(SendKnockResponse::new(dedup_stripped_state(response.knock_room_state)))
}

#[implement(Service)]
async fn make_knock_request(
	&self,
	sender_user: &UserId,
	room_id: &RoomId,
	servers: &[OwnedServerName],
) -> Result<(federation::membership::prepare_knock_event::v1::Response, OwnedServerName)> {
	let mut make_knock_response_and_server =
		Err!(BadServerResponse("No server available to assist in knocking."));

	let mut make_knock_counter: usize = 0;

	for remote_server in servers {
		if self
			.services
			.globals
			.server_is_ours(remote_server)
		{
			continue;
		}

		info!("Asking {remote_server} for make_knock ({make_knock_counter})");

		let make_knock_response = self
			.services
			.federation
			.execute(remote_server, federation::membership::prepare_knock_event::v1::Request {
				room_id: room_id.to_owned(),
				user_id: sender_user.to_owned(),
				ver: self
					.services
					.config
					.supported_room_versions()
					.map(at!(0))
					.collect(),
			})
			.await;

		trace!("make_knock response: {make_knock_response:?}");
		make_knock_counter = make_knock_counter.saturating_add(1);

		make_knock_response_and_server = make_knock_response.map(|r| (r, remote_server.clone()));

		if make_knock_response_and_server.is_ok() {
			break;
		}

		if make_knock_counter > 40 {
			warn!(
				"50 servers failed to provide valid make_knock response, assuming no server can \
				 assist in knocking."
			);
			make_knock_response_and_server =
				Err!(BadServerResponse("No server available to assist in knocking."));

			return make_knock_response_and_server;
		}
	}

	make_knock_response_and_server
}

/// Builds the room summary a knock records for the knocking user.
///
/// A full PDU is shown only once its signatures and content hash verify and it
/// belongs to the room; one that fails is omitted. Legacy entries pass as hints,
/// a create the MSC4311 policy drops is omitted in either form, and the user's
/// own membership entry is always the knock this server signed.
#[implement(Service)]
#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
async fn knock_summary(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	knock_pdu: &PduEvent,
	knock_room_state: Vec<RawStrippedState>,
) -> Result<Summary> {
	let pdus = knock_room_state
		.iter()
		.filter_map(|state| match state {
			| RawStrippedState::Pdu(pdu) => Some(pdu),
			| RawStrippedState::Stripped(_) => None,
		});

	self.services
		.server_keys
		.acquire_events_pubkeys(pdus)
		.await;

	let verdict = self
		.validate_stripped_create(&knock_room_state, room_id, room_version_id)
		.await?;

	let enforce = self
		.services
		.config
		.federation
		.enforce_stripped_state_pdu_validation;

	let drop_create = enforce_stripped_create(verdict, v12_room_ids(room_version_id), enforce);

	if verdict != StrippedCreateVerdict::Valid {
		debug_warn!(?verdict, %room_id, drop_create, "MSC4311 knock create-event validation failed");
	}

	let rules = room_version_rules(room_version_id)?;
	let summary = without_member(knock_room_state, knock_pdu.sender())
		.filter(|state| !drop_create || !occupies_cell(state, &StateEventType::RoomCreate, ""))
		.stream()
		.wide_filter_map(|state| {
			self.knock_summary_entry(room_id, room_version_id, &rules, state)
		})
		.chain(once(knock_pdu.to_format()).stream())
		.collect()
		.await;

	Ok(summary)
}

#[implement(Service)]
#[expect(
	deprecated,
	reason = "Matrix 1.16 still permits receiving the legacy stripped variant for backwards \
	          compatibility."
)]
async fn knock_summary_entry(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	rules: &RoomVersionRules,
	state: RawStrippedState,
) -> Option<Raw<AnyStrippedStateEvent>> {
	let pdu = match state {
		| RawStrippedState::Stripped(raw) => return Some(raw),
		| RawStrippedState::Pdu(pdu) => pdu,
	};

	let (event_id, pdu, _, verified) = self
		.verified_pdu(room_id, room_version_id, rules, &pdu)
		.inspect_err(
			|error| debug_warn!(%room_id, %error, "Omitting an invalid knock summary event."),
		)
		.map(Result::ok)
		.await?;

	if verified != Verified::All {
		debug_warn!(%room_id, %event_id, "Omitting a knock summary event failing its content hash.");
		return None;
	}

	Some(pdu.to_format())
}

#[implement(Service)]
async fn record_knock(
	&self,
	room_id: &RoomId,
	sender_user: &UserId,
	pdu: &PduEvent,
	summary: Summary,
) -> Result {
	info!("Updating membership locally to knock state with the room summary");
	let count = self.services.globals.next_count();
	let membership_event = pdu.get_content().expect("we just created this");

	// A remote knock appends no event, so its unread counts are cleared here.
	self.services
		.pusher
		.reset_notification_counts_for_thread(
			sender_user,
			room_id,
			None,
			&ReceiptThread::Unthreaded,
		)
		.await;

	self.services
		.sending
		.refresh_push_badge(sender_user)
		.await
		.log_err()
		.ok();

	self.services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id,
			user_id: sender_user,
			membership_event,
			sender: sender_user,
			last_state: Some(summary),
			invite_via: None,
			update_joined_count: false,
			count: PduCount::Normal(*count),
		})
		.await
}

#[cfg(test)]
mod tests;
