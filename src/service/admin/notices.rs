use futures::FutureExt;
use ruma::{RoomId, events::room::message::RoomMessageEventContent};
use tuwunel_core::{Result, implement};

/// Sends a notice gated on the admin_room_notices config; admin command
/// responses use the unconditional notice().
#[implement(super::Service)]
pub async fn notify(&self, body: &str) {
	if self.services.server.config.admin_room_notices {
		self.notice(body).await;
	}
}

/// Sends a message gated on the admin_room_notices config; admin command
/// responses use the unconditional send_text().
#[implement(super::Service)]
pub async fn notify_loud(&self, body: &str) {
	if self.services.server.config.admin_room_notices {
		self.send_text(body).await;
	}
}

/// Sends markdown notice to the admin room as the admin user.
#[implement(super::Service)]
pub async fn notice(&self, body: &str) {
	self.send_message(RoomMessageEventContent::notice_markdown(body))
		.await
		.ok();
}

/// Sends markdown message (not an m.notice for notification reasons) to the
/// admin room as the admin user.
#[implement(super::Service)]
pub async fn send_text(&self, body: &str) {
	self.send_message(RoomMessageEventContent::text_markdown(body))
		.await
		.ok();
}

/// Sends a markdown report to the configured report room, falling back to
/// the admin room, as the server user.
#[implement(super::Service)]
pub async fn send_report(&self, body: &str) {
	let Ok(room_id) = self.get_report_room().await else {
		return;
	};

	self.send_to_room(RoomMessageEventContent::text_markdown(body), &room_id)
		.await
		.ok();
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
