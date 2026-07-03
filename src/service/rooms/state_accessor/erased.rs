use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, RoomId, ServerName, UserId,
	canonical_json::redact_in_place, events::room::member::MembershipState,
};
use tuwunel_core::{
	implement,
	matrix::{Event, Pdu},
	utils::{
		BoolExt,
		result::{FlatOk, LogErr},
	},
};

/// MSC4025: the pruned clone of `pdu` for recipient `user_id`, or `None` to
/// serve the original.
#[implement(super::Service)]
pub async fn erased_view(&self, user_id: &UserId, pdu: &Pdu) -> Option<Pdu> {
	self.erased_for(user_id, pdu)
		.await
		.then_async(|| self.pruned(pdu))
		.await
		.flatten()
}

/// MSC4025: whether `pdu` serves pruned to `user_id`: its sender is erased
/// and the recipient was not joined in the room state at the event.
#[implement(super::Service)]
pub async fn erased_for(&self, user_id: &UserId, pdu: &Pdu) -> bool {
	self.services.users.is_erased(pdu.sender()).await
		&& self.user_membership_at_pdu(user_id, pdu).await != MembershipState::Join
}

/// MSC4025: prune a federation-served event: its sender is erased and
/// `origin` had no user joined in the room state at the event. Composes with
/// history visibility rather than replacing it.
#[implement(super::Service)]
pub async fn erased_for_server(
	&self,
	origin: &ServerName,
	mut pdu: CanonicalJsonObject,
) -> CanonicalJsonObject {
	if !self
		.services
		.config
		.enforce_erasure_over_federation
	{
		return pdu;
	}

	let sender = pdu
		.get("sender")
		.and_then(CanonicalJsonValue::as_str)
		.map(<&UserId>::try_from)
		.flat_ok();

	let Some(sender) = sender else {
		return pdu;
	};

	if !self.services.users.is_erased(sender).await {
		return pdu;
	}

	let event_id = pdu
		.get("event_id")
		.and_then(CanonicalJsonValue::as_str)
		.map(<&EventId>::try_from)
		.flat_ok();

	let Some(event_id) = event_id else {
		return pdu;
	};

	if self.server_joined_at_pdu(origin, event_id).await {
		return pdu;
	}

	let room_id = pdu
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.map(<&RoomId>::try_from)
		.flat_ok();

	let Some(room_id) = room_id else {
		return pdu;
	};

	let Ok(rules) = self
		.services
		.state
		.get_room_version_rules(room_id)
		.await
		.log_err()
	else {
		return pdu;
	};

	redact_in_place(&mut pdu, &rules.redaction, None)
		.log_err()
		.ok();

	pdu
}

/// Prune per the room version's redaction rules. The pruned form carries no
/// `redacted_because`; no redaction event exists for a serve-time erasure.
#[implement(super::Service)]
async fn pruned(&self, pdu: &Pdu) -> Option<Pdu> {
	let rules = self
		.services
		.state
		.get_room_version_rules(pdu.room_id())
		.await
		.log_err()
		.ok()?;

	pdu.redacted(&rules.redaction).log_err().ok()
}
