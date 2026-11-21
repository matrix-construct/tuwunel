use ruma::{CanonicalJsonObject, api::error::ErrorKind, event_id, room_id};
use serde_json::{from_value, json};
use tuwunel_core::{Result, config::Figment};

use crate::test_utils::fixture;

/// The entry for events this server built refuses one sent by another server's
/// user, before it reads or stores anything.
///
/// The room does not exist, so `Forbidden` rather than not-found shows the
/// sender check runs before the create-event read.
#[tokio::test]
async fn a_local_pdu_from_another_server_is_refused() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let pdu: CanonicalJsonObject = from_value(json!({ "sender": "@mallory:remote.example" }))?;
	let refused = fixture
		.services
		.event_handler
		.handle_local_pdu(room_id!("!room:localhost"), event_id!("$event"), pdu, None)
		.await;

	assert!(
		refused
			.as_ref()
			.is_err_and(|error| matches!(error.kind(), ErrorKind::Forbidden)),
		"{refused:?}"
	);

	Ok(())
}
