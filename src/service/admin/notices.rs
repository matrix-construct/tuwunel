use futures::FutureExt;
use ruma::{RoomId, events::room::message::RoomMessageEventContent};
use tuwunel_core::{Result, implement};

/// Sends a notice gated on `admin.room.notices`.
///
/// Admin command responses use the unconditional `notice()`.
#[implement(super::Service)]
pub async fn notify(&self, body: &str) {
	if self.services.server.config.admin.room.notices {
		self.notice(body).await;
	}
}

/// Sends a message gated on `admin.room.notices`.
///
/// Admin command responses use the unconditional `send_text()`.
#[implement(super::Service)]
pub async fn notify_loud(&self, body: &str) {
	if self.services.server.config.admin.room.notices {
		self.send_text(body).await;
	}
}

/// Sends markdown notice to the admin room as the admin user.
#[implement(super::Service)]
pub async fn notice(&self, body: &str) {
	self.send_message(notice_markdown(body))
		.await
		.ok();
}

/// Sends markdown message (not an m.notice for notification reasons) to the
/// admin room as the admin user.
#[implement(super::Service)]
pub async fn send_text(&self, body: &str) { self.send_message(text_markdown(body)).await.ok(); }

/// Sends a markdown report to the configured report room, falling back to
/// the admin room, as the server user.
#[implement(super::Service)]
pub async fn send_report(&self, body: &str) {
	self.send_report_content(text_markdown(body))
		.await;
}

/// Sends prepared report content to the configured report room.
///
/// Falls back to the admin room and sends as the server user. The supplied
/// plaintext and formatted bodies are passed through without Markdown parsing.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn send_report_content(&self, content: RoomMessageEventContent) {
	let Ok(room_id) = self.get_report_room().await else {
		return;
	};

	self.send_to_room(content, &room_id).await.ok();
}

/// Sends a message to the admin room as the admin user (see send_text() for
/// convenience).
#[implement(super::Service)]
pub async fn send_message(&self, message_content: RoomMessageEventContent) -> Result {
	let room_id = self.get_admin_room().await?;

	self.send_to_room(message_content, &room_id).await
}

#[implement(super::Service)]
async fn send_to_room(&self, content: RoomMessageEventContent, room_id: &RoomId) -> Result {
	let user_id = &self.services.globals.server_user;

	self.respond_to_room(content, room_id, user_id)
		.boxed()
		.await
}

/// Builds an `m.notice` from Markdown source.
///
/// The body keeps the source as given. Builds with the `markdown` feature add
/// an HTML formatted body when the source uses Markdown syntax.
#[cfg(feature = "markdown")]
pub(super) fn notice_markdown(body: &str) -> RoomMessageEventContent {
	RoomMessageEventContent::notice_markdown(body)
}

#[cfg(not(feature = "markdown"))]
pub(super) fn notice_markdown(body: &str) -> RoomMessageEventContent {
	RoomMessageEventContent::notice_plain(body)
}

/// Builds an `m.text` message from Markdown source.
///
/// The body keeps the source as given. Builds with the `markdown` feature add
/// an HTML formatted body when the source uses Markdown syntax.
#[cfg(feature = "markdown")]
pub(super) fn text_markdown(body: &str) -> RoomMessageEventContent {
	RoomMessageEventContent::text_markdown(body)
}

#[cfg(not(feature = "markdown"))]
pub(super) fn text_markdown(body: &str) -> RoomMessageEventContent {
	RoomMessageEventContent::text_plain(body)
}
