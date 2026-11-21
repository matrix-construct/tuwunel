use http::StatusCode;
use ruma::{
	OwnedServerName,
	api::{
		client::delayed_events::get_delayed_event::v1::Finalized,
		error::{ErrorBody, ErrorKind, LimitExceededErrorData, StandardErrorBody},
	},
};
use tuwunel_core::{Error, Result};

use super::{Event, outcome};

const SENT: &str = r#","finalised":{"finalised_ts":3000,"event_id":"$sent:example.org"}"#;
const FAILED: &str = r#","finalised":{"finalised_ts":3000,"error":{"errcode":"M_LIMIT_EXCEEDED","retry_after_ms":5000,"error":"Too many."}}"#;
const CANCELLED: &str = r#","finalised":{"finalised_ts":3000}"#;
const REMOTE: &str = "remote.example";

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

#[test]
fn remote_error_the_response_withholds_stays_out_of_the_outcome() -> Result {
	let Finalized::Error { error, .. } = outcome(&Err(remote_limit_exceeded()?)) else {
		panic!("a failed send finalises as an error");
	};

	assert_eq!(error.kind, ErrorKind::Unknown, "the remote's errcode is withheld: {error:?}");
	assert!(error.message.contains(REMOTE), "the remote is named: {error:?}");

	Ok(())
}

/// A rate limit the remote answered with, which a client would take for its own.
fn remote_limit_exceeded() -> Result<Error> {
	let origin = OwnedServerName::try_from(REMOTE)?;
	let kind = ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after: None });
	let body = ErrorBody::Standard(StandardErrorBody::new(kind, "Too many.".to_owned()));

	Ok(Error::Federation(origin, body.into_error(StatusCode::TOO_MANY_REQUESTS)))
}
