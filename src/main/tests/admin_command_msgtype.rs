#![cfg(test)]

use std::time::Duration;

use serde_json::json;
use tuwunel_core::{
	Result,
	pdu::PduBuilder,
	ruma::{OwnedEventId, RoomId, UserId, events::room::message::RoomMessageEventContent},
};
use tuwunel_service::Services;

use self::{
	client::{Client, poll_until, register},
	fixture::boot,
};

mod client;
mod fixture;

const ADMIN_TOKEN: &str = "admin-command-msgtype-admin-access-token";

const COMMAND_DEADLINE: Duration = Duration::from_secs(10);

/// Only an admin's typed `m.text` runs as a command, escaped or not.
///
/// The spec forbids answering a notice automatically, and a bot relaying a
/// stranger's text from an admin's account posts it as a notice. The worker
/// runs commands in arrival order, so once the last typed command has run,
/// any other message taken as a command would already have deactivated its
/// user.
#[test]
fn only_typed_text_runs_as_a_command() -> Result {
	let options = [
		"create_admin_room=true",
		"grant_admin_to_first_user=false",
		"admin_escape_commands=true",
	];

	boot("admin-command-msgtype", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let admin_id = register(services, "cmdadmin", ADMIN_TOKEN).await?;
	let user = async |name: &str| {
		let token = format!("admin-command-msgtype-{name}-access-token");

		register(services, name, &token).await
	};

	let escaped_notice_target = user("noticeescaped").await?;
	let prefixed_notice_target = user("noticeprefixed").await?;
	let escaped_emote_target = user("emoteescaped").await?;
	let escaped_text_target = user("textescaped").await?;
	let prefixed_text_target = user("textprefixed").await?;

	services.admin.make_user_admin(&admin_id).await?;

	let admin = Client { services, base, token: ADMIN_TOKEN };
	let public_room = admin
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let admin_room = services.admin.get_admin_room().await?;
	let command = |prefix: &str, user_id: &UserId| format!("{prefix} users deactivate {user_id}");
	let escaped_notice =
		RoomMessageEventContent::notice_plain(command("\\!admin", &escaped_notice_target));

	let prefixed_notice =
		RoomMessageEventContent::notice_plain(command("!admin", &prefixed_notice_target));

	let escaped_emote =
		RoomMessageEventContent::emote_plain(command("\\!admin", &escaped_emote_target));

	let escaped_text =
		RoomMessageEventContent::text_plain(command("\\!admin", &escaped_text_target));

	let prefixed_text =
		RoomMessageEventContent::text_plain(command("!admin", &prefixed_text_target));

	append_message(services, &admin_id, &public_room, &escaped_notice).await?;
	append_message(services, &admin_id, &admin_room, &prefixed_notice).await?;
	append_message(services, &admin_id, &public_room, &escaped_emote).await?;
	append_message(services, &admin_id, &public_room, &escaped_text).await?;
	append_message(services, &admin_id, &admin_room, &prefixed_text).await?;

	let deactivated = async |user_id: &UserId| services.users.is_deactivated(user_id).await;
	let typed_ran = async || {
		deactivated(&prefixed_text_target)
			.await
			.unwrap_or(false)
	};

	let ran = poll_until(COMMAND_DEADLINE, typed_ran).await;

	assert!(ran, "the typed admin-room command never ran");
	assert!(deactivated(&escaped_text_target).await?, "the typed escape never ran");
	assert!(!deactivated(&escaped_notice_target).await?, "an escaped notice ran");
	assert!(!deactivated(&prefixed_notice_target).await?, "an admin-room notice ran");
	assert!(!deactivated(&escaped_emote_target).await?, "an escaped emote ran");

	Ok(())
}

/// Append a message to a room as `sender` and return its event id.
///
/// The event goes straight through the timeline service under the room's
/// state lock, so any msgtype can be sent without a client round trip.
async fn append_message(
	services: &Services,
	sender: &UserId,
	room_id: &RoomId,
	content: &RoomMessageEventContent,
) -> Result<OwnedEventId> {
	let builder = PduBuilder::timeline(content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, sender, room_id, &state_lock)
		.await
}
