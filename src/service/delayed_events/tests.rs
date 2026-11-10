use ruma::api::{
	client::delayed_events::get_delayed_event::v1::Finalized,
	error::{ErrorKind, StandardErrorBody},
};
use tuwunel_core::Result;

use super::Event;

const SENT: &str = r#","finalised":{"finalised_ts":3000,"event_id":"$sent:example.org"}"#;
const FAILED: &str = r#","finalised":{"finalised_ts":3000,"error":{"errcode":"M_LIMIT_EXCEEDED","retry_after_ms":5000,"error":"Too many."}}"#;
const CANCELLED: &str = r#","finalised":{"finalised_ts":3000}"#;

#[test]
fn scheduled_record_reads_without_outcome() -> Result {
	let event: Event = serde_json::from_str(&record(""))?;

	assert!(event.finalised.is_none(), "{event:?}");

	Ok(())
}

fn record(finalised: &str) -> String {
	format!(
		r#"{{"user_id":"@alice:example.org","room_id":"!room:example.org","type":"m.room.message","content":{{"body":"x"}},"delay_ms":1000,"due_ms":2000{finalised}}}"#
	)
}

#[test]
fn outcomes_round_trip() -> Result {
	for finalised in [SENT, FAILED, CANCELLED] {
		let stored = record(finalised);
		let event: Event = serde_json::from_str(&stored)?;
		let decoded = match (finalised, &event.finalised) {
			| (SENT, Some(Finalized::Sent { event_id, .. })) => event_id == "$sent:example.org",
			| (FAILED, Some(Finalized::Error { error, .. })) => limit_exceeded(error),
			| (CANCELLED, Some(Finalized::Canceled { .. })) => true,
			| _ => false,
		};

		assert!(decoded, "{stored} decoded as {event:?}");
		assert_eq!(serde_json::to_string(&event)?, stored);
	}

	Ok(())
}

fn limit_exceeded(error: &StandardErrorBody) -> bool {
	matches!(&error.kind, ErrorKind::LimitExceeded(data) if data.retry_after.is_some())
}
