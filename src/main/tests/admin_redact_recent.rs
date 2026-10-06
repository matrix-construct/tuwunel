#![cfg(test)]

use serde_json::{Value, from_str, from_value, json};
use tuwunel_core::{
	Result,
	matrix::{Event, pdu::PduBuilder},
	ruma::{
		EventId, OwnedEventId, RoomId, UserId,
		events::{TimelineEventType, room::message::RoomMessageEventContent},
	},
};
use tuwunel_service::Services;

use self::{
	admin::{accepted, refused},
	client::{Client, register},
	fixture::boot,
};

mod admin;
mod client;
mod fixture;

const TOKEN: &str = "redact-recent-owner-access-token-for-test";

#[test]
fn redacts_recent_messages_from_one_local_user() -> Result {
	boot("admin-redact-recent", ["create_admin_room=true"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let owner = register(services, "redactor", TOKEN).await?;
	let other =
		register(services, "bystander", "redact-recent-other-access-token-for-test").await?;

	let client = Client { services, base, token: TOKEN };
	let room = client
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	let elsewhere = client
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	accepted(services, &format!("users force-join-room {other} {room}")).await?;
	let oldest = send(services, &owner, &room, message("oldest")).await?;
	let latest = send(services, &owner, &room, message("latest")).await?;
	let bystander = send(services, &other, &room, message("bystander")).await?;
	let content = from_value(json!({
		"algorithm": "m.megolm.v1.aes-sha2",
		"ciphertext": "opaque",
		"sender_key": "key",
		"device_id": "device",
		"session_id": "session"
	}))?;

	let encrypted = PduBuilder {
		event_type: TimelineEventType::RoomEncrypted,
		content,
		..Default::default()
	};

	let encrypted = send(services, &owner, &room, encrypted).await?;
	let state = PduBuilder {
		state_key: Some("".into()),
		..message("state stays")
	};

	let state = send(services, &owner, &room, state).await?;
	let isolated = send(services, &owner, &elsewhere, message("other room")).await?;

	refused(services, &format!("users redact-recent {owner} {room} 0"), "invalid value").await?;
	refused(
		services,
		&format!("users redact-recent @remote:elsewhere.invalid {room} 1"),
		"does not belong to our server",
	)
	.await?;

	refused(
		services,
		&format!("users redact-recent absent {room} 1"),
		"does not exist on this server",
	)
	.await?;

	assert_redacted(services, &latest, false).await?;
	assert_redacted(services, &encrypted, false).await?;
	accepted(services, &format!("users redact-recent redactor {room} 2")).await?;
	assert_redacted(services, &latest, true).await?;
	assert_redacted(services, &encrypted, true).await?;
	assert_redacted(services, &oldest, false).await?;
	assert_redacted(services, &bystander, false).await?;
	assert_redacted(services, &state, false).await?;
	assert_redacted(services, &isolated, false).await?;
	accepted(services, &format!("users redact-recent {owner} {room} 1")).await?;
	assert_redacted(services, &oldest, true).await?;
	accepted(services, &format!("users redact-recent {owner} {room} 20")).await?;
	assert_redacted(services, &bystander, false).await?;
	assert_redacted(services, &state, false).await?;
	assert_redacted(services, &isolated, false).await?;
	accepted(services, &format!("users redact-event {isolated}")).await?;
	assert_redacted(services, &isolated, true).await?;
	Ok(())
}

fn message(body: &str) -> PduBuilder {
	PduBuilder::timeline(&RoomMessageEventContent::text_plain(body))
}

async fn send(
	services: &Services,
	sender: &UserId,
	room: &RoomId,
	builder: PduBuilder,
) -> Result<OwnedEventId> {
	let lock = services.state.mutex.lock(room).await;

	services
		.timeline
		.build_and_append_pdu(builder, sender, room, &lock)
		.await
}

async fn assert_redacted(services: &Services, event_id: &EventId, expected: bool) -> Result {
	let event = services
		.timeline
		.get_non_outlier_pdu(event_id)
		.await?;

	assert_eq!(event.is_redacted(), expected, "redaction status of {event_id}");
	if expected {
		let unsigned = event.unsigned().expect("redaction metadata");
		let unsigned: Value = from_str(unsigned.get())?;

		assert_eq!(
			unsigned["redacted_because"]["sender"],
			event.sender().as_str(),
			"redaction sender of {event_id}"
		);
	}

	Ok(())
}
