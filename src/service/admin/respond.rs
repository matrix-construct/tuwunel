use futures::FutureExt;
use ruma::{
	EventId, RoomId, UserId,
	events::{
		relation::{InReplyTo, Reply as ReplyRelation},
		room::message::{Relation, RoomMessageEventContent},
	},
};
use tuwunel_core::{Error, Event, Result, error, error::default_log, implement, pdu::PduBuilder};

use super::CommandOutput;
use crate::rooms::state::RoomMutexGuard;

#[implement(super::Service)]
pub(super) async fn handle_response(
	&self,
	output: CommandOutput,
	reply_id: Option<&EventId>,
) -> Result {
	let Some(reply_id) = reply_id else {
		return Ok(());
	};

	let Ok(pdu) = self.services.timeline.get_pdu(reply_id).await else {
		error!(?reply_id, "Missing admin command in_reply_to event");
		return Ok(());
	};

	let response_sender = if self.is_admin_room(pdu.room_id()).await {
		&self.services.globals.server_user
	} else {
		pdu.sender()
	};

	let content = render(output, reply_id);

	self.respond_to_room(content, pdu.room_id(), response_sender)
		.boxed()
		.await
}

fn render(output: CommandOutput, reply_id: &EventId) -> RoomMessageEventContent {
	let mut content = match output {
		| CommandOutput::Markdown(text) => RoomMessageEventContent::notice_markdown(text),
		| CommandOutput::Plain(text) => RoomMessageEventContent::notice_plain(text),
	};

	content.relates_to = Some(Relation::Reply(ReplyRelation {
		in_reply_to: InReplyTo { event_id: reply_id.to_owned() },
	}));

	content
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
