#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	ruma::{
		CanonicalJsonObject, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, RoomVersionId,
		UserId,
		events::{
			AnyStrippedStateEvent,
			room::member::{MembershipState, RoomMemberEventContent},
		},
		serde::Raw,
	},
};
use tuwunel_matrix::{PduCount, pdu::into_outgoing_federation};
use tuwunel_service::{Services, rooms::state_cache::MembershipUpdate};

use self::{
	client::{Client, field, register},
	fixture::boot,
};

#[expect(dead_code)] // This test creates no rooms through the client API.
mod client;
mod fixture;
mod sliding_sync;

#[path = "sliding_sync/resume.rs"]
mod resume;

const INVITER_TOKEN: &str = "sync-v5-rescinded-invite-inviter-access-token";
const INVITEE_TOKEN: &str = "sync-v5-rescinded-invite-invitee-access-token";
const ROOM_VERSION: RoomVersionId = RoomVersionId::V11;

/// A federated invite rescission reaches sync as the invitee's own leave.
///
/// The server is not resident in the room, so no room state holds a member
/// event for the invitee. Sliding-sync clients read the departure from that
/// event rather than from `membership`, so the retracted room must carry one,
/// as the legacy left room already does.
#[test]
fn rescinded_invite_carries_leave_member() -> Result {
	let options: [&str; 0] = [];

	boot("sync-v5-rescinded-invite", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let inviter = register(services, "rescinder", INVITER_TOKEN).await?;
	let invitee = register(services, "rescindee", INVITEE_TOKEN).await?;
	let client = Client { services, base, token: INVITEE_TOKEN };
	let room_id = RoomId::parse("!rescinded:remote.test")?;
	let room = room_id.as_str();
	let request = list("rescind");

	plant_invite(services, &room_id, &inviter, &invitee).await?;

	let invited = client.sync(None, &request).await?;
	let invited_v3 = client.sync_v3(None).await?;

	assert_eq!(invited["rooms"][room]["membership"], "invite", "invite missing: {invited}");

	let (event_id, kick) = sign_kick(services, &room_id, &inviter, &invitee)?;

	services
		.event_handler
		.handle_incoming_pdu(services.globals.server_name(), &room_id, &event_id, kick, true)
		.await?;

	let membership = services
		.state_cache
		.user_membership(&invitee, &room_id)
		.await;

	assert_eq!(membership, Some(MembershipState::Leave), "rescission not applied");

	let left = client.sync(Some(&invited), &request).await?;
	let member = own_member(&left["rooms"][room]["required_state"], &invitee);

	assert_eq!(left["rooms"][room]["membership"], "leave", "room not retracted: {left}");
	assert_eq!(member["content"]["membership"], "leave", "no own leave event: {left}");

	let since = field(&invited_v3, "next_batch")?;
	let left_v3 = client.sync_v3(Some(since)).await?;
	let member = own_member(&left_v3["rooms"]["leave"][room]["state"]["events"], &invitee);

	assert_eq!(member["content"]["membership"], "leave", "no legacy leave event: {left_v3}");

	Ok(())
}

fn list(connection: &str) -> Value {
	json!({
		"conn_id": connection,
		"lists": { "all": {
			"ranges": [[0, 9]], "timeline_limit": 1,
			"required_state": [["m.room.member", "$ME"]]
		} },
	})
}

/// Store an out-of-band invite the way a federated invite leaves it.
///
/// Its stripped state names the room version and the inviter, and no room
/// state is written.
async fn plant_invite(
	services: &Services,
	room_id: &RoomId,
	inviter: &UserId,
	invitee: &UserId,
) -> Result {
	let create = json!({
		"type": "m.room.create",
		"state_key": "",
		"sender": inviter,
		"content": { "room_version": ROOM_VERSION },
	});

	let member = json!({
		"type": "m.room.member",
		"state_key": invitee,
		"sender": inviter,
		"content": { "membership": "invite" },
	});

	let last_state: Vec<Raw<AnyStrippedStateEvent>> = [create, member]
		.into_iter()
		.map(serde_json::from_value)
		.collect::<Result<_, _>>()?;

	services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id,
			user_id: invitee,
			membership_event: RoomMemberEventContent::new(MembershipState::Invite),
			sender: inviter,
			last_state: Some(last_state),
			invite_via: None,
			update_joined_count: false,
			count: PduCount::Normal(*services.globals.next_count()),
		})
		.await
}

#[implement(Client, params = "<'_>")]
async fn sync_v3(&self, since: Option<&str>) -> Result<Value> {
	let since = since
		.map(|since| format!("&since={since}"))
		.unwrap_or_default();

	let response = self
		.services
		.client
		.clients
		.default
		.get(self.url(&format!("sync?timeout=0{since}")))
		.bearer_auth(self.token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(response)
}

/// The inviter's kick of the invitee, signed by the inviter's server.
///
/// The inviter is local, so that server is this one and the kick verifies
/// against its own key, as a remote inviter's would against the remote's.
fn sign_kick(
	services: &Services,
	room_id: &RoomId,
	inviter: &UserId,
	invitee: &UserId,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
	let kick = json!({
		"type": "m.room.member",
		"state_key": invitee,
		"sender": inviter,
		"room_id": room_id,
		"content": { "membership": "leave", "reason": "withdrawn" },
		"origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
		"depth": 2,
		"prev_events": [],
		"auth_events": [],
	});

	let mut kick: CanonicalJsonObject = serde_json::from_value(kick)?; // signer takes &mut
	let event_id = services
		.server_keys
		.gen_id_hash_and_sign_event(&mut kick, &ROOM_VERSION)?;

	Ok((event_id, into_outgoing_federation(kick, &ROOM_VERSION)))
}

/// The invitee's own membership event among a section's events.
///
/// An absent event reads as null, so the caller's assertion reports it.
fn own_member<'a>(events: &'a Value, invitee: &UserId) -> &'a Value {
	events
		.as_array()
		.into_iter()
		.flatten()
		.find(|event| event["type"] == "m.room.member" && event["state_key"] == invitee.as_str())
		.unwrap_or(&Value::Null)
}
