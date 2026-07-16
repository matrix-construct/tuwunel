use std::collections::BTreeMap;

use axum::extract::State;
use futures::{StreamExt, TryStreamExt};
use ruma::{
	EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UInt,
	UserId,
	events::{
		TimelineEventType,
		room::{
			member::{MembershipState, RoomMemberEventContent},
			redaction::RoomRedactionEventContent,
		},
	},
};
use serde_json::Value as JsonValue;
use synapse_admin_api::users::redact::v1::{Request, Response};
use tuwunel_core::{
	Err, Result,
	matrix::{
		Event,
		pdu::{PduBuilder, PduEvent},
	},
	utils::{
		BoolExt,
		stream::{BroadbandExt, TryReadyExt},
	},
};

use crate::{Ruma, client::admin::require_admin};

/// Synapse defaults an omitted or zero limit to 1000 events per room.
const LIMIT_DEFAULT: usize = 1000;

type FailedRedactions = BTreeMap<OwnedEventId, String>;

struct RedactArgs {
	user_id: OwnedUserId,
	rooms: Vec<OwnedRoomId>,
	redact_as: OwnedUserId,
	reason: Option<String>,
	limit: usize,
	before_ts: Option<MilliSecondsSinceUnixEpoch>,
	after_ts: Option<MilliSecondsSinceUnixEpoch>,
}

/// # `POST /_synapse/admin/v1/user/{user_id}/redact`
///
/// Schedules a background task redacting the user's messages, encrypted events,
/// and join events in the given rooms (all joined and banned rooms when empty),
/// returning the task id for the redact-status endpoint.
pub(crate) async fn admin_redact_user_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let user_id = &body.user_id;

	let in_progress = services
		.tasks
		.has_nonterminal(super::REDACT_USER_ACTION, user_id.as_str());

	if in_progress {
		return Err!(Request(InvalidParam("Redact already in progress for user {user_id}")));
	}

	let rooms = body
		.rooms
		.is_empty()
		.then_async(async || joined_and_banned_rooms(&services, user_id).await)
		.await;

	let redact_as = (body.use_admin || !services.globals.user_is_local(user_id))
		.then(|| body.sender_user().to_owned());

	let Request {
		user_id,
		rooms: requested_rooms,
		reason,
		limit,
		before_ts,
		after_ts,
		..
	} = body.body;

	let rooms = rooms.unwrap_or(requested_rooms);
	let resource_id = user_id.to_string();
	let redact_as = redact_as.unwrap_or_else(|| user_id.clone());

	let args = RedactArgs {
		user_id,
		rooms,
		redact_as,
		reason,
		limit: resolve_limit(limit),
		before_ts,
		after_ts,
	};

	let redact_id = services
		.tasks
		.spawn(super::REDACT_USER_ACTION, resource_id, redact_events(services, args))
		.to_string();

	Ok(Response { redact_id })
}

async fn joined_and_banned_rooms(services: &crate::State, user_id: &UserId) -> Vec<OwnedRoomId> {
	let joined = services
		.state_cache
		.rooms_joined(user_id)
		.map(ToOwned::to_owned);

	let banned = services
		.state_cache
		.rooms_left(user_id)
		.map(ToOwned::to_owned)
		.broad_filter_map(async |room_id| {
			services
				.state_accessor
				.get_member(&room_id, user_id)
				.await
				.is_ok_and(|member| member.membership == MembershipState::Ban)
				.then_some(room_id)
		});

	joined.chain(banned).collect().await
}

fn resolve_limit(limit: Option<UInt>) -> usize {
	limit
		.and_then(|limit| limit.try_into().ok())
		.filter(|&limit| limit > 0)
		.unwrap_or(LIMIT_DEFAULT)
}

// The detached task needs its own static services handle.
async fn redact_events(services: crate::State, args: RedactArgs) -> Result<JsonValue> {
	let mut failed = FailedRedactions::new();

	for room_id in &args.rooms {
		let event_ids: Vec<OwnedEventId> = services
			.timeline
			.pdus_rev(None, room_id, None)
			.ready_try_filter(|(_, pdu)| is_candidate(pdu, &args))
			.take(args.limit)
			.ready_try_filter(|(_, pdu)| is_eligible(pdu))
			.map_ok(|(_, pdu)| pdu.event_id)
			.try_collect()
			.await?;

		for event_id in event_ids {
			if let Err(e) = redact_one(&services, room_id, &event_id, &args).await {
				failed.insert(event_id, e.to_string());
			}
		}
	}

	Ok(task_result(&failed))
}

fn is_candidate(pdu: &PduEvent, args: &RedactArgs) -> bool {
	pdu.sender == args.user_id
		&& args
			.before_ts
			.is_none_or(|ts| pdu.origin_server_ts() <= ts)
		&& args
			.after_ts
			.is_none_or(|ts| pdu.origin_server_ts() >= ts)
		&& matches!(
			pdu.kind,
			TimelineEventType::RoomMember
				| TimelineEventType::RoomMessage
				| TimelineEventType::RoomEncrypted
		)
}

fn is_eligible(pdu: &PduEvent) -> bool {
	!pdu.is_redacted()
		&& (pdu.kind != TimelineEventType::RoomMember
			|| pdu
				.get_content()
				.is_ok_and(|member: RoomMemberEventContent| {
					member.membership == MembershipState::Join
				}))
}

async fn redact_one(
	services: &crate::State,
	room_id: &RoomId,
	event_id: &EventId,
	args: &RedactArgs,
) -> Result<()> {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				redacts: Some(event_id.to_owned()),
				..PduBuilder::timeline(&RoomRedactionEventContent {
					redacts: Some(event_id.to_owned()),
					reason: args.reason.clone(),
				})
			},
			&args.redact_as,
			room_id,
			&state_lock,
		)
		.await
		.map(|_| ())
}

fn task_result(failed_redactions: &FailedRedactions) -> JsonValue {
	serde_json::json!({ "failed_redactions": failed_redactions })
}

#[cfg(test)]
mod tests {
	use ruma::{MilliSecondsSinceUnixEpoch, UInt, event_id, uint, user_id};
	use serde_json::json;

	use super::{
		FailedRedactions, PduEvent, RedactArgs, is_candidate, is_eligible, resolve_limit,
		task_result,
	};

	fn args(before_ts: Option<UInt>, after_ts: Option<UInt>) -> RedactArgs {
		RedactArgs {
			user_id: user_id!("@alice:example.com").to_owned(),
			rooms: Vec::new(),
			redact_as: user_id!("@alice:example.com").to_owned(),
			reason: None,
			limit: super::LIMIT_DEFAULT,
			before_ts: before_ts.map(MilliSecondsSinceUnixEpoch),
			after_ts: after_ts.map(MilliSecondsSinceUnixEpoch),
		}
	}

	fn pdu(kind: &str, sender: &str, ts: u64, content: &serde_json::Value) -> PduEvent {
		pdu_with_unsigned(kind, sender, ts, content, &json!({}))
	}

	fn pdu_with_unsigned(
		kind: &str,
		sender: &str,
		ts: u64,
		content: &serde_json::Value,
		unsigned: &serde_json::Value,
	) -> PduEvent {
		serde_json::from_value(json!({
			"type": kind,
			"content": content,
			"event_id": "$e:example.com",
			"room_id": "!room:example.com",
			"sender": sender,
			"prev_events": ["$prev:example.com"],
			"auth_events": ["$auth:example.com"],
			"origin_server_ts": ts,
			"depth": 12,
			"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
			"unsigned": unsigned,
		}))
		.expect("valid pdu")
	}

	fn redacted_message() -> PduEvent {
		pdu_with_unsigned(
			"m.room.message",
			"@alice:example.com",
			1000,
			&json!({}),
			&json!({ "redacted_because": {} }),
		)
	}

	#[test]
	fn resolve_limit_defaults_zero_and_absent_to_1000() {
		assert_eq!(resolve_limit(None), super::LIMIT_DEFAULT);
		assert_eq!(resolve_limit(Some(uint!(0))), super::LIMIT_DEFAULT);
		assert_eq!(resolve_limit(Some(uint!(25))), 25);
	}

	#[test]
	fn task_result_always_carries_the_failed_redactions_key() {
		assert_eq!(task_result(&FailedRedactions::new()), json!({ "failed_redactions": {} }));

		let one: FailedRedactions =
			[(event_id!("$f:example.com").to_owned(), "boom".to_owned())].into();

		let value = task_result(&one);

		assert_eq!(value, json!({ "failed_redactions": { "$f:example.com": "boom" } }));

		let parsed: FailedRedactions = serde_json::from_value(value["failed_redactions"].clone())
			.expect("failed_redactions round-trips");

		assert_eq!(parsed.len(), 1);
		assert!(parsed.contains_key(event_id!("$f:example.com")));
	}

	#[test]
	fn candidate_filters_sender_type_and_window() {
		let args = args(Some(uint!(1500)), Some(uint!(500)));

		assert!(is_candidate(
			&pdu("m.room.message", "@alice:example.com", 1000, &json!({})),
			&args
		));
		assert!(!is_candidate(
			&pdu("m.room.message", "@mallory:example.com", 1000, &json!({})),
			&args
		));
		assert!(!is_candidate(
			&pdu("m.room.topic", "@alice:example.com", 1000, &json!({})),
			&args
		));

		// The window is inclusive at both bounds.
		assert!(is_candidate(
			&pdu("m.room.message", "@alice:example.com", 1500, &json!({})),
			&args
		));
		assert!(is_candidate(
			&pdu("m.room.message", "@alice:example.com", 500, &json!({})),
			&args
		));
		assert!(!is_candidate(
			&pdu("m.room.message", "@alice:example.com", 1501, &json!({})),
			&args
		));
		assert!(!is_candidate(
			&pdu("m.room.message", "@alice:example.com", 499, &json!({})),
			&args
		));
	}

	#[test]
	fn eligible_keeps_joins_and_skips_redacted() {
		let join =
			pdu("m.room.member", "@alice:example.com", 1000, &json!({ "membership": "join" }));

		let leave =
			pdu("m.room.member", "@alice:example.com", 1000, &json!({ "membership": "leave" }));

		let invite =
			pdu("m.room.member", "@alice:example.com", 1000, &json!({ "membership": "invite" }));

		let message = pdu("m.room.message", "@alice:example.com", 1000, &json!({}));

		assert!(is_eligible(&join));
		assert!(!is_eligible(&leave));
		assert!(!is_eligible(&invite));
		assert!(is_eligible(&message));
		assert!(!is_eligible(&redacted_message()));
	}
}
