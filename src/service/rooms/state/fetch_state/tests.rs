use ruma::{event_id, events::StateEventType};
use tuwunel_core::{Error, Result, config::Figment};

use super::IdMapState;
use crate::{rooms::state_res::FetchState, test_utils::fixture};

#[tokio::test]
async fn id_map_distinguishes_absent_state_from_missing_events() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let event_type = StateEventType::RoomTopic;
	let shortstatekey = services
		.short
		.get_or_create_shortstatekey(&event_type, "")
		.await;

	let ids = [(shortstatekey, event_id!("$missing-state-event:localhost").to_owned())].into();
	let state = IdMapState { services, ids: &ids };
	let missing_event = state.get(event_type, "".into()).await;

	assert!(matches!(missing_event, Err(Error::Database(..))));

	let absent_state = state
		.get(StateEventType::RoomName, "uninterned-state-key".into())
		.await;

	assert!(absent_state.is_err_and(|error| error.is_not_found()));

	Ok(())
}
