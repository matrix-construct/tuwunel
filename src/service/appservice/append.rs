use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{UserId, events::TimelineEventType};
use tuwunel_core::{
	Result, error, implement,
	utils::{
		ReadyExt,
		stream::{IterStream, automatic_width},
	},
};
use tuwunel_matrix::{
	event::Event,
	pdu::{Pdu, RawPduId},
};

use super::RegistrationInfo;

/// Called by timeline::append() after accepting new PDU.
#[implement(super::Service)]
#[tracing::instrument(name = "append", level = "debug", skip_all)]
pub(crate) async fn append_pdu(&self, pdu_id: RawPduId, pdu: &Pdu) -> Result {
	let guard = self.read().await;

	guard
		.values()
		.stream()
		.for_each_concurrent(automatic_width(), |appservice| {
			self.append_pdu_to(appservice, pdu_id, pdu)
				.inspect_err(|e| {
					error!(
						event_id = %pdu.event_id(),
						appservice = ?appservice.registration.id,
						"Failed to send PDU to appservice: {e}"
					);
				})
				.map(drop)
		})
		.await;

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
	if self.should_append_to(appservice, pdu).await {
		self.services
			.sending
			.send_pdu_appservice(appservice.registration.id.clone(), pdu_id)?;
	}

	Ok(())
}

#[implement(super::Service)]
async fn should_append_to(&self, appservice: &RegistrationInfo, pdu: &Pdu) -> bool {
	if self
		.services
		.state_cache
		.appservice_in_room(pdu.room_id(), appservice)
		.await
	{
		return true;
	}

	if appservice.is_user_match(pdu.sender()) {
		return true;
	}

	if *pdu.kind() == TimelineEventType::RoomMember
		&& pdu
			.state_key
			.as_ref()
			.and_then(|state_key| UserId::parse(state_key.as_str()).ok())
			.is_some_and(|user_id| appservice.is_user_match(&user_id))
	{
		return true;
	}

	if self
		.services
		.alias
		.local_aliases_for_room(pdu.room_id())
		.ready_any(|room_alias| appservice.aliases.is_match(room_alias.as_str()))
		.await
	{
		return true;
	}

	if appservice.rooms.is_match(pdu.room_id().as_str()) {
		return true;
	}

	false
}
