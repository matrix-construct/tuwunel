use ruma::{
	MilliSecondsSinceUnixEpoch, RoomVersionId, event_id, room_id, server_name, uint, user_id,
};
use serde_json::{from_value, json};
use tuwunel_core::{Error, Result, config::Figment};

use super::super::{
	backoff::{Context, Disposition},
	eval::Eval,
};
use crate::test_utils::{fixture, pdu_json};

/// A previous event whose copy names another ID than the one walked to is not
/// upgraded, and is held back once its failure is recorded.
///
/// The upgrade resolves state for the walked ID, so the mismatch fails as a
/// storage error before any upgrade work, but after the retry gate.
#[tokio::test]
async fn a_previous_event_naming_another_id_is_not_upgraded() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let room_id = room_id!("!walked:localhost");
	let prev_id = event_id!("$prev");
	let sender = user_id!("@bob:localhost");
	let copy = pdu_json(event_id!("$other"), room_id, sender, "m.room.message", &json!({}));
	let eval = Eval {
		origin: server_name!("localhost"),
		room_id,
		event_id: event_id!("$incoming"),
		room_version: &RoomVersionId::V11,
		create_event_id: event_id!("$create"),
		recursion_level: 0,
		auth_events_known: false,
	};

	let handler = &fixture.services.event_handler;
	let first_normal_ts = MilliSecondsSinceUnixEpoch(uint!(0));
	let upgrade = async || {
		let info = (from_value(copy.clone())?, from_value(copy.clone())?);

		handler
			.handle_prev_pdu(eval, first_normal_ts, Some(info), prev_id)
			.await
	};

	let upgraded = upgrade().await;

	assert!(matches!(upgraded, Err(Error::Database(_))), "{upgraded:?}");

	handler.record_outcome(Context::Upgrade, prev_id, Disposition::Transient);

	let retried = upgrade().await;

	assert!(matches!(retried, Ok(None)), "{retried:?}");

	Ok(())
}
