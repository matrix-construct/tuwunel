use ruma::owned_event_id;

use super::StateIdsResponse;

#[test]
fn state_ids_keep_each_event_once() {
	let response: StateIdsResponse =
		serde_json::from_str(r#"{"pdu_ids":["$one","$two","$one"]}"#).expect("valid state IDs");
	let ids = response.pdu_ids;

	assert_eq!(ids.len(), 2, "expected one entry per distinct event");
	for id in [owned_event_id!("$one"), owned_event_id!("$two")] {
		assert!(ids.contains(&id), "missing state ID {id}");
	}
}
