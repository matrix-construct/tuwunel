use std::io;

use futures::{FutureExt, TryStreamExt};
use ruma::{
	EventId, OwnedEventId, RoomId, UserId,
	events::{
		relation::{InReplyTo, Reply as ReplyRelation, Thread},
		room::{
			encrypted::Relation as EncryptedRelation,
			message::{
				Relation, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
			},
		},
	},
};
use serde::Deserialize;
use tuwunel_core::{
	Error, Event, Result, error,
	error::default_log,
	implement,
	pdu::{MAX_PDU_BYTES, PduBuilder},
	utils::{stream::IterStream, string::chunk},
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

/// Serialized size reserved for an `m.relates_to` relation carrying a
/// maximum-length event id, so a segment measured without its relation still
/// fits once the relation is attached.
const RELATION_RESERVE: usize = 512;

/// Largest serialized segment content, before its relation, that fits an event.
const SEGMENT_BUDGET: usize = CONTENT_BUDGET - RELATION_RESERVE;

/// How a command's output events relate back to the command event.
enum Mode {
	/// Thread children rooted at the given event (the command or its own
	/// thread root).
	Thread(OwnedEventId),

	/// Replies: the first to the command, each subsequent to the one before it.
	Reply(OwnedEventId),
}

#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: EncryptedRelation,
}

/// An `io::Write` sink that counts bytes without buffering them, for measuring
/// serialized length.
#[derive(Default)]
struct Counter(usize);

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

	let threads = self.services.server.config.admin_output_threads;
	let mode = command_thread(&pdu)
		.or_else(|| threads.then(|| reply_id.to_owned()))
		.map_or_else(|| Mode::Reply(reply_id.to_owned()), Mode::Thread);

	self.respond(&output, pdu.room_id(), response_sender, &mode)
		.boxed()
		.await
}

/// Splits the output across up to `admin_output_max_events` reply or thread
/// events; output needing more events, or any output when the limit is zero,
/// is uploaded and posted as a single file attachment instead.
#[implement(super::Service)]
async fn respond(
	&self,
	output: &CommandOutput,
	room_id: &RoomId,
	sender: &UserId,
	mode: &Mode,
) -> Result {
	let markdown = matches!(output, CommandOutput::Markdown(_));
	let max_events = self
		.services
		.server
		.config
		.admin_output_max_events;

	let segments = (max_events != 0)
		.then(|| {
			chunk(output.as_str(), markdown, |text: &str| fits_segment(text, markdown))
				.take(max_events.saturating_add(1))
				.collect::<Vec<_>>()
		})
		.filter(|segments| segments.len() <= max_events);

	match segments {
		| Some(segments) =>
			self.send_segments(&segments, room_id, sender, mode, markdown)
				.boxed()
				.await,
		| None => {
			let mut content = self.attach(output).await?;
			content.relates_to = Some(mode.relation(None));

			self.respond_to_room(content, room_id, sender)
				.boxed()
				.await
		},
	}
}

/// Appends every segment under one room lock, chaining each reply to the event
/// id returned by the previous append.
#[implement(super::Service)]
async fn send_segments(
	&self,
	segments: &[String],
	room_id: &RoomId,
	sender: &UserId,
	mode: &Mode,
	markdown: bool,
) -> Result {
	assert!(self.user_is_admin(sender).await, "sender is not admin");

	let state_lock = self.services.state.mutex.lock(room_id).await;

	let result = segments
		.iter()
		.try_stream()
		.try_fold(None, async |previous: Option<OwnedEventId>, segment| {
			let mut content = notice(segment, markdown);
			content.relates_to = Some(mode.relation(previous.as_deref()));

			self.services
				.timeline
				.build_and_append_pdu(
					PduBuilder::timeline(&content),
					sender,
					room_id,
					&state_lock,
				)
				.await
				.map(Some)
		})
		.await;

	if let Err(e) = result {
		self.handle_response_error(e, room_id, sender, &state_lock)
			.boxed()
			.await
			.unwrap_or_else(default_log);
	}

	Ok(())
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

/// The thread root of the command event when it was itself sent inside a
/// thread, so the output joins that thread rather than starting a new relation.
fn command_thread(pdu: &impl Event) -> Option<OwnedEventId> {
	pdu.get_content()
		.ok()
		.and_then(|content: ExtractRelatesTo| match content.relates_to {
			| EncryptedRelation::Thread(thread) => Some(thread.event_id),
			| _ => None,
		})
}

fn fits_segment(text: &str, markdown: bool) -> bool {
	let mut counter = Counter::default();

	serde_json::to_writer(&mut counter, &notice(text, markdown))
		.is_ok_and(|()| counter.0 <= SEGMENT_BUDGET)
}

impl Mode {
	/// The relation for the next event: a thread child of the root, or a reply
	/// to the previous segment when there is one, else to the command.
	fn relation(&self, previous: Option<&EventId>) -> MessageRelation {
		match self {
			| Self::Thread(root) => thread_relation(root),
			| Self::Reply(command) => reply_relation(previous.unwrap_or(command)),
		}
	}
}

fn notice(text: &str, markdown: bool) -> RoomMessageEventContent {
	match markdown {
		| true => RoomMessageEventContent::notice_markdown(text),
		| false => RoomMessageEventContent::notice_plain(text),
	}
}

impl io::Write for Counter {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.0 = self.0.saturating_add(buf.len());

		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn thread_relation(root: &EventId) -> MessageRelation {
	Relation::Thread(Thread::without_fallback(root.to_owned()))
}

fn reply_relation(event_id: &EventId) -> MessageRelation {
	Relation::Reply(ReplyRelation {
		in_reply_to: InReplyTo { event_id: event_id.to_owned() },
	})
}

#[cfg(test)]
mod tests {
	use ruma::{EventId, ID_MAX_BYTES};

	use super::{RELATION_RESERVE, notice, reply_relation, thread_relation};

	/// The reserve must cover the largest relation (a maximum-length event id),
	/// so a segment measured without its relation still fits once attached.
	#[test]
	fn relation_reserve_covers_max_length_event_id() {
		let raw = format!("${}", "a".repeat(ID_MAX_BYTES.saturating_sub(1)));
		let event_id = <&EventId>::try_from(raw.as_str()).expect("valid max-length event id");
		let bare = serde_json::to_string(&notice("body", true))
			.expect("serialize")
			.len();

		for relation in [reply_relation(event_id), thread_relation(event_id)] {
			let mut content = notice("body", true);
			content.relates_to = Some(relation);

			let full = serde_json::to_string(&content)
				.expect("serialize")
				.len();

			assert!(full.saturating_sub(bare) <= RELATION_RESERVE);
		}
	}
}
