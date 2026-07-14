use futures::FutureExt;
use ruma::{
	EventId, RoomId, UserId,
	events::{
		relation::{InReplyTo, Reply as ReplyRelation},
		room::message::{
			Relation, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
		},
	},
};
use tuwunel_core::{
	Error, Event, Result, error,
	error::default_log,
	implement,
	pdu::{MAX_PDU_BYTES, PduBuilder},
};

use super::CommandOutput;
use crate::rooms::state::RoomMutexGuard;

/// Room event relation carried by an admin response.
type MessageRelation = Relation<RoomMessageEventContentWithoutRelation>;

/// Envelope overhead reserved above the message content: prev and auth event
/// ids at their maximum, 255-byte sender and room ids, and the signatures
/// block, just under 10 KiB in the worst legal case.
const EVENT_RESERVE: usize = 10_240;

/// Largest serialized message content that still fits a single event.
const CONTENT_BUDGET: usize = MAX_PDU_BYTES - EVENT_RESERVE;

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

	let content = self.render_content(&output, reply_id).await?;

	self.respond_to_room(content, pdu.room_id(), response_sender)
		.boxed()
		.await
}

/// Renders the output as a single reply event when it fits and replies are
/// permitted, otherwise uploads it as a file attachment.
#[implement(super::Service)]
async fn render_content(
	&self,
	output: &CommandOutput,
	reply_id: &EventId,
) -> Result<RoomMessageEventContent> {
	let max_events = self
		.services
		.server
		.config
		.admin_output_max_events;

	// The raw text is a cheap lower bound: rendering only grows it, so output
	// already over budget skips straight to an attachment.
	let single = (max_events != 0 && output.as_str().len() <= CONTENT_BUDGET)
		.then(|| render(output, reply_id))
		.filter(fits_one_event);

	match single {
		| Some(content) => Ok(content),
		| None => {
			let mut content = self.attach(output).await?;
			content.relates_to = Some(reply_relation(reply_id));

			Ok(content)
		},
	}
}

fn render(output: &CommandOutput, reply_id: &EventId) -> RoomMessageEventContent {
	let mut content = match output {
		| CommandOutput::Markdown(text) =>
			RoomMessageEventContent::notice_markdown(text.as_str()),
		| CommandOutput::Plain(text) => RoomMessageEventContent::notice_plain(text.as_str()),
	};

	content.relates_to = Some(reply_relation(reply_id));

	content
}

fn reply_relation(reply_id: &EventId) -> MessageRelation {
	Relation::Reply(ReplyRelation {
		in_reply_to: InReplyTo { event_id: reply_id.to_owned() },
	})
}

fn fits_one_event(content: &RoomMessageEventContent) -> bool {
	serde_json::to_string(content).is_ok_and(|json| json.len() <= CONTENT_BUDGET)
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
