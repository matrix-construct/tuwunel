use futures::FutureExt;
use ruma::{
	RoomId, UserId,
	events::{
		relation::Reply as ReplyRelation,
		room::message::{Relation, RoomMessageEventContent},
	},
};
use tuwunel_core::{Error, Event, Result, error, error::default_log, implement, pdu::PduBuilder};

use crate::rooms::state::RoomMutexGuard;

#[implement(super::Service)]
pub(super) async fn handle_response(&self, content: RoomMessageEventContent) -> Result {
	let Some(Relation::Reply(ReplyRelation { in_reply_to })) = content.relates_to.as_ref() else {
		return Ok(());
	};

	let Ok(pdu) = self
		.services
		.timeline
		.get_pdu(&in_reply_to.event_id)
		.await
	else {
		error!(
			event_id = ?in_reply_to.event_id,
			"Missing admin command in_reply_to event"
		);
		return Ok(());
	};

	let response_sender = if self.is_admin_room(pdu.room_id()).await {
		&self.services.globals.server_user
	} else {
		pdu.sender()
	};

	self.respond_to_room(content, pdu.room_id(), response_sender)
		.boxed()
		.await
}

#[implement(super::Service)]
pub(super) async fn respond_to_room(
	&self,
	content: RoomMessageEventContent,
	room_id: &RoomId,
	user_id: &UserId,
) -> Result {
	assert!(self.user_is_admin(user_id).await, "sender is not admin");

	let state_lock = self.services.state.mutex.lock(room_id).await;

	if let Err(e) = self
		.services
		.timeline
		.build_and_append_pdu(PduBuilder::timeline(&content), user_id, room_id, &state_lock)
		.await
	{
		self.handle_response_error(e, room_id, user_id, &state_lock)
			.boxed()
			.await
			.unwrap_or_else(default_log);
	}

	Ok(())
}

#[implement(super::Service)]
async fn handle_response_error(
	&self,
	e: Error,
	room_id: &RoomId,
	user_id: &UserId,
	state_lock: &RoomMutexGuard,
) -> Result {
	error!(%e, "Failed to build and append admin room response PDU");
	let content = RoomMessageEventContent::text_plain(format!(
		"Failed to build and append admin room PDU: \"{e}\"\n\nThe original admin command may \
		 have finished successfully, but we could not return the output."
	));

	self.services
		.timeline
		.build_and_append_pdu(PduBuilder::timeline(&content), user_id, room_id, state_lock)
		.boxed()
		.await?;

	Ok(())
}
