mod auto_accept;
mod ban;
mod invite;
mod join;
mod kick;
mod knock;
mod leave;
mod stripped_state;
mod unban;

use std::sync::Arc;

use async_trait::async_trait;
use loole::{Receiver, Sender, unbounded};
use ruma::{
	CanonicalJsonObject, OwnedEventId, RoomId, RoomVersionId, UserId,
	events::room::member::MembershipState, room_version_rules::RoomVersionRules,
	signatures::Verified,
};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Result, err, implement};
use tuwunel_matrix::pdu::Pdu;

use self::auto_accept::Pending;
pub use self::{
	join::Join,
	stripped_state::{
		StrippedCreateVerdict, dedup_stripped_state, enforce_stripped_create,
		into_client_stripped, v12_room_ids, without_member,
	},
};

/// A verified federation event bound to its room.
///
/// It carries the event ID, the parsed PDU, the stored form and how it verified.
type VerifiedPdu = (OwnedEventId, Pdu, CanonicalJsonObject, Verified);

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	queue: (Sender<Pending>, Receiver<Pending>),
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			queue: unbounded(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.accept_worker().await;

		Ok(())
	}

	async fn interrupt(&self) {
		let (sender, _) = &self.queue;

		if !sender.is_closed() {
			sender.close();
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Whether a room this server is in holds a ban on the user.
///
/// A room we left can lift a ban without telling us, so its frozen state
/// answers for nothing here and the room itself refuses or admits the user. A
/// failed residency read keeps the state's answer.
#[implement(Service)]
async fn banned_from_resident_room(&self, user_id: &UserId, room_id: &RoomId) -> bool {
	let resident = self
		.services
		.state_cache
		.server_in_room_result(self.services.globals.server_name(), room_id)
		.await
		.unwrap_or(true);

	resident
		&& self
			.services
			.state_accessor
			.get_member(room_id, user_id)
			.await
			.is_ok_and(|content| content.membership == MembershipState::Ban)
}

/// Verifies a federation event and binds it to the room as a PDU.
///
/// Only signing keys already stored are used. The event comes back in the form
/// this server stores it, with its verification classification.
#[implement(Service)]
async fn verified_pdu(
	&self,
	room_id: &RoomId,
	room_version_id: &RoomVersionId,
	room_version_rules: &RoomVersionRules,
	pdu: &RawJsonValue,
) -> Result<VerifiedPdu> {
	let (event_id, value, verified) = self
		.services
		.server_keys
		.validate_and_add_event_id_no_fetch(pdu, room_version_id)
		.await?;

	let (pdu, value) = Pdu::from_object_federation(room_id, &event_id, value, room_version_rules)
		.map_err(|error| err!("Invalid PDU {event_id}: {error}"))?;

	Ok((event_id, pdu, value, verified))
}
