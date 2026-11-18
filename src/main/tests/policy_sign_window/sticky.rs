//! MSC4354 with MSC4284: a sticky event withheld only by its room's state
//! loses a sticky event's visibility once the room's policy server no longer
//! signs it.
//!
//! The visibility in question is the one a server joining after the event
//! gains while the event is sticky; the stub is the room's policy server.

use std::time::Duration;

use serde_json::{json, value::to_raw_value};
use tuwunel_core::{
	Result,
	ruma::{
		RoomId, ServerName, UserId,
		events::room::{
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			member::{MembershipState, RoomMemberEventContent},
			policy::RoomPolicyEventContent,
		},
		serde::Base64,
	},
};
use tuwunel_matrix::{event::Sticky, pdu::PduBuilder};

use super::{
	Harness, MESSAGE, client::poll_until, federation::send_as, key_pair, sign::sign,
	state_cache::membership_update, transaction,
};

// Long enough to outlast the case, so only the policy ends the event's stickiness.
const STICKY_MS: u64 = 600_000;

// Far past the worker's usual delay, so a slow host is not mistaken for a kept
// visibility.
const REFUSAL_DEADLINE: Duration = Duration::from_secs(10);

// A server that joins after the event, which only stickiness lets see it.
const LATER: &str = "later.invalid";

/// A withheld sticky event the room's policy server stops signing is no longer
/// visible to a server that joined after it.
///
/// Bob's event is built while he is joined and delivered after Alice bans him,
/// so only the room's current state withholds it. The policy change then weighs
/// it again, and the stub refuses it.
pub(super) async fn a_refused_withheld_event_loses_its_visibility(
	h: &Harness<'_>,
	room_id: &RoomId,
) -> Result {
	let joined = RoomHistoryVisibilityEventContent::new(HistoryVisibility::Joined);

	h.append(room_id, &h.alice_id, PduBuilder::state("", &joined))
		.await?;

	let content = json!({ "msgtype": "m.text", "body": "late", "msc4354_sticky_key": "late" });
	let builder = PduBuilder {
		event_type: MESSAGE.into(),
		content: to_raw_value(&content)?.into(),
		sticky: Some(Sticky { duration_ms: STICKY_MS }),
		..PduBuilder::default()
	};

	let (event, json) = sign(h.services, &h.bob_id, room_id, builder).await?;
	let ban = RoomMemberEventContent::new(MembershipState::Ban);

	h.append(room_id, &h.alice_id, PduBuilder::state(h.bob_id.as_str(), &ban))
		.await?;

	let pdu = h
		.services
		.federation
		.format_pdu_into(json, None)
		.await;

	let request = transaction(h.policy.name.clone(), pdu);
	let (status, body) =
		send_as(h.services, h.alice.base, &h.policy.name, &h.policy.server_key, request).await?;

	assert_eq!(status, 200, "the transaction was refused: {body}");
	assert!(
		h.services
			.pdu_metadata
			.is_event_soft_failed(&event.event_id)
			.await,
		"the late event was not withheld",
	);

	let later = ServerName::parse(LATER)?;
	let member = UserId::parse(format!("@member:{LATER}"))?;
	let count = h.services.globals.next_count();
	let arrival = membership_update(room_id, &member, MembershipState::Join, *count);

	h.services
		.state_cache
		.update_membership(arrival)
		.await?;

	drop(count);

	let visible = async || {
		h.services
			.state_accessor
			.server_can_see_event(&later, room_id, &event.event_id)
			.await
	};

	assert!(visible().await, "a later server could not see the withheld sticky event");

	let other = key_pair("other")?;
	let public_key = Base64::new(other.public_key().to_vec());
	let policy = RoomPolicyEventContent::new(h.policy.name.clone(), public_key);

	h.append(room_id, &h.alice_id, PduBuilder::state("", &policy))
		.await?;

	h.policy.refuse(room_id);

	let lost = poll_until(REFUSAL_DEADLINE, async || !visible().await).await;

	assert!(
		lost,
		"a withheld event its policy server refused kept a sticky event's visibility"
	);

	Ok(())
}
