use ruma::{UserId, events::TimelineEventType};
use tuwunel_core::{
	Result, error, implement,
	matrix::{
		event::Event,
		pdu::{Pdu, RawPduId},
	},
	utils::ReadyExt,
};

use super::{NamespaceRegex, RegistrationInfo};

/// Called by timeline::append() after accepting new PDU.
#[implement(super::Service)]
#[tracing::instrument(name = "append", level = "debug", skip_all)]
pub(crate) async fn append_pdu(&self, pdu_id: RawPduId, pdu: &Pdu) -> Result {
	for appservice in self.read().await.values() {
		self.append_pdu_to(appservice, pdu_id, pdu)
			.await
			.inspect_err(|e| {
				error!(
					event_id = %pdu.event_id(),
					appservice = ?appservice.registration.id,
					"Failed to send PDU to appservice: {e}"
				);
			})
			.ok();
	}

	Ok(())
}

#[implement(super::Service)]
#[tracing::instrument(
	name = "append_to",
	level = "debug",
	skip_all,
	fields(id = %appservice.registration.id),
)]
async fn append_pdu_to(
	&self,
	appservice: &RegistrationInfo,
	pdu_id: RawPduId,
	pdu: &Pdu,
) -> Result {
	if self
		.services
		.state_cache
		.appservice_in_room(pdu.room_id(), appservice)
		.await
	{
		self.services
			.sending
			.send_pdu_appservice(appservice.registration.id.clone(), pdu_id)?;

		return Ok(());
	}

	// If the RoomMember event has a non-empty state_key, it is targeted at someone.
	// If it is our appservice user, we send this PDU to it.
	if *pdu.kind() == TimelineEventType::RoomMember {
		if let Some(state_key_uid) = &pdu
			.state_key
			.as_ref()
			.and_then(|state_key| UserId::parse(state_key.as_str()).ok())
		{
			let appservice_uid = appservice.registration.sender_localpart.as_str();
			if state_key_uid == &appservice_uid {
				self.services
					.sending
					.send_pdu_appservice(appservice.registration.id.clone(), pdu_id)?;

				return Ok(());
			}
		}
	}

	let matching_users = |users: &NamespaceRegex| {
		appservice.users.is_match(pdu.sender().as_str())
			|| *pdu.kind() == TimelineEventType::RoomMember
				&& pdu
					.state_key
					.as_ref()
					.is_some_and(|state_key| users.is_match(state_key))
	};
	let matching_aliases = |aliases: NamespaceRegex| {
		self.services
			.alias
			.local_aliases_for_room(pdu.room_id())
			.ready_any(move |room_alias| aliases.is_match(room_alias.as_str()))
	};

	if matching_aliases(appservice.aliases.clone()).await
		|| appservice.rooms.is_match(pdu.room_id().as_str())
		|| matching_users(&appservice.users)
	{
		self.services
			.sending
			.send_pdu_appservice(appservice.registration.id.clone(), pdu_id)?;
	}

	Ok(())
}
