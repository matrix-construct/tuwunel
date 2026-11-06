#![cfg(test)]

use tuwunel_core::{
	Result,
	ruma::{
		CanonicalJsonValue, MilliSecondsSinceUnixEpoch, events::TimelineEventType, room_id,
		serde::Raw, uint,
	},
};
use tuwunel_matrix::{StateKey, pdu::PduBuilder};
use tuwunel_service::Services;

use self::{client::register, fixture::boot};

#[expect(dead_code)]
mod client;
mod fixture;

#[test]
fn same_millisecond_creates_have_distinct_ids() -> Result {
	boot("create-room-nonce", [], exercise)
}

async fn exercise(services: &Services, _base: &str) -> Result {
	let sender = register(services, "nonce", "create-room-nonce-regression-access-token").await?;
	let placeholder = room_id!("!thiswillbereplaced");
	let timestamp = MilliSecondsSinceUnixEpoch(uint!(1_000));
	let content =
		[("room_version".into(), "12".into()), ("uwu.nonce".into(), "supplied".into())].into();

	let state_lock = services.state.mutex.lock(placeholder).await;
	let create = async || {
		let builder = PduBuilder {
			event_type: TimelineEventType::RoomCreate,
			content: Raw::new(&content)?,
			state_key: Some(StateKey::new()),
			timestamp: Some(timestamp),
			..Default::default()
		};

		services
			.timeline
			.create_hash_and_sign_event(builder, &sender, placeholder, &state_lock)
			.await
	};

	let (first, first_json) = create().await?;
	let (second, second_json) = create().await?;

	assert_ne!(first.event_id, second.event_id);
	assert_ne!(first.room_id, second.room_id);
	assert_ne!(first_json["content"], second_json["content"]);

	for (pdu, canonical) in [(first, first_json), (second, second_json)] {
		let content = pdu.content.deserialize()?;
		let nonce = content["uwu.nonce"]
			.as_str()
			.expect("nonce string");

		assert_ne!(nonce, "supplied");
		assert_eq!(nonce.len(), 32);
		assert_eq!(pdu.origin_server_ts, timestamp.get());
		assert_eq!(canonical["origin_server_ts"], CanonicalJsonValue::from(timestamp.get()));
		assert_eq!(canonical["content"], content);
	}

	Ok(())
}
