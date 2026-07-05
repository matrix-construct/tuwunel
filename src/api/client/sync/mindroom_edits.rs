use std::collections::{HashMap, HashSet};

use ruma::{OwnedEventId, OwnedUserId};
use tuwunel_core::{
	PduCount,
	matrix::{
		event::{Event, ExtractRelatesToInfo},
		pdu::PduEvent,
	},
};

/// Collapse multiple m.replace relation events targeting the same event from
/// the same sender into just the latest one by timeline order (`PduCount`),
/// with `event_id` as a deterministic tie-break.
///
/// Non-replace events are always kept. For each `(target_event_id, sender)`
/// group with multiple replacements in the batch, only the newest replacement
/// is retained.
pub(super) fn collapse_superseded_edits(
	events: Vec<(PduCount, PduEvent)>,
) -> Vec<(PduCount, PduEvent)> {
	let mut replace_events_by_target_sender: HashMap<(OwnedEventId, OwnedUserId), Vec<usize>> =
		HashMap::new();

	for (idx, (_, pdu)) in events.iter().enumerate() {
		if let Ok(content) = pdu.get_content::<ExtractRelatesToInfo>()
			&& content.relates_to.rel_type == "m.replace"
		{
			replace_events_by_target_sender
				.entry((content.relates_to.event_id.clone(), pdu.sender.clone()))
				.or_default()
				.push(idx);
		}
	}

	if replace_events_by_target_sender
		.values()
		.all(|indices| indices.len() <= 1)
	{
		return events;
	}

	let mut remove_indices: HashSet<usize> = HashSet::new();
	for indices in replace_events_by_target_sender.values() {
		if indices.len() <= 1 {
			continue;
		}

		let best_idx = *indices
			.iter()
			.max_by(|&&a, &&b| {
				let (count_a, pdu_a) = &events[a];
				let (count_b, pdu_b) = &events[b];
				count_a
					.cmp(count_b)
					.then_with(|| pdu_a.event_id.cmp(&pdu_b.event_id))
			})
			.expect("indices is non-empty");

		for &idx in indices {
			if idx != best_idx {
				remove_indices.insert(idx);
			}
		}
	}

	events
		.into_iter()
		.enumerate()
		.filter(|(idx, _)| !remove_indices.contains(idx))
		.map(|(_, item)| item)
		.collect()
}

#[cfg(test)]
mod tests {
	use ruma::{EventId, OwnedRoomId, OwnedUserId, UInt};
	use serde_json::value::RawValue;
	use tuwunel_core::matrix::pdu::{EventHash, PduCount, PduEvent};

	use super::collapse_superseded_edits;

	fn make_pdu(event_id: &str, ts: u64, replace_target: Option<&str>) -> (PduCount, PduEvent) {
		make_pdu_with_sender(event_id, ts, replace_target, "@user:example.com")
	}

	fn make_pdu_with_sender(
		event_id: &str,
		ts: u64,
		replace_target: Option<&str>,
		sender: &str,
	) -> (PduCount, PduEvent) {
		make_pdu_with_count_and_sender(event_id, ts, ts, replace_target, sender)
	}

	fn make_pdu_with_count_and_sender(
		event_id: &str,
		count: u64,
		ts: u64,
		replace_target: Option<&str>,
		sender: &str,
	) -> (PduCount, PduEvent) {
		let content = replace_target.map_or_else(
			|| r#"{"body":"hello"}"#.to_owned(),
			|target| {
				format!(
					r#"{{"body":"edited","m.relates_to":{{"rel_type":"m.replace","event_id":"{target}"}}}}"#
				)
			},
		);

		let pdu = PduEvent {
			kind: ruma::events::TimelineEventType::RoomMessage,
			content: RawValue::from_string(content)
				.expect("valid JSON")
				.into(),
			event_id: EventId::parse(event_id).expect("valid event_id"),
			room_id: OwnedRoomId::try_from("!test:example.com").expect("valid room_id"),
			sender: OwnedUserId::try_from(sender).expect("valid user_id"),
			state_key: None,
			redacts: None,
			prev_events: Default::default(),
			auth_events: Default::default(),
			origin_server_ts: UInt::try_from(ts).expect("valid ts"),
			depth: UInt::try_from(1_u64).expect("valid depth"),
			hashes: EventHash::default(),
			origin: None,
			unsigned: None,
		};

		(PduCount::Normal(count), pdu)
	}

	#[test]
	fn collapse_no_edits() {
		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_pdu("$msg2:example.com", 2000, None),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 2);
	}

	#[test]
	fn collapse_single_edit_kept() {
		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_pdu("$edit1:example.com", 2000, Some("$msg1:example.com")),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 2);
		assert_eq!(result[1].1.event_id.as_str(), "$edit1:example.com");
	}

	#[test]
	fn collapse_multiple_edits_keeps_latest() {
		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_pdu("$edit1:example.com", 2000, Some("$msg1:example.com")),
			make_pdu("$edit2:example.com", 3000, Some("$msg1:example.com")),
			make_pdu("$edit3:example.com", 4000, Some("$msg1:example.com")),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 2);
		assert_eq!(result[0].1.event_id.as_str(), "$msg1:example.com");
		assert_eq!(result[1].1.event_id.as_str(), "$edit3:example.com");
	}

	#[test]
	fn collapse_multiple_targets_independent() {
		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_pdu("$edit1a:example.com", 2000, Some("$msg1:example.com")),
			make_pdu("$edit1b:example.com", 3000, Some("$msg1:example.com")),
			make_pdu("$msg2:example.com", 4000, None),
			make_pdu("$edit2a:example.com", 5000, Some("$msg2:example.com")),
			make_pdu("$edit2b:example.com", 6000, Some("$msg2:example.com")),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 4);

		let event_ids: Vec<&str> = result
			.iter()
			.map(|(_, pdu)| pdu.event_id.as_str())
			.collect();
		assert!(event_ids.contains(&"$msg1:example.com"));
		assert!(event_ids.contains(&"$edit1b:example.com"));
		assert!(event_ids.contains(&"$msg2:example.com"));
		assert!(event_ids.contains(&"$edit2b:example.com"));
		assert!(!event_ids.contains(&"$edit1a:example.com"));
		assert!(!event_ids.contains(&"$edit2a:example.com"));
	}

	#[test]
	fn collapse_preserves_order() {
		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_pdu("$edit1:example.com", 2000, Some("$msg1:example.com")),
			make_pdu("$msg2:example.com", 3000, None),
			make_pdu("$edit2:example.com", 4000, Some("$msg1:example.com")),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 3);
		assert_eq!(result[0].1.event_id.as_str(), "$msg1:example.com");
		assert_eq!(result[1].1.event_id.as_str(), "$msg2:example.com");
		assert_eq!(result[2].1.event_id.as_str(), "$edit2:example.com");
	}

	#[test]
	fn collapse_same_ts_uses_event_id_tiebreak() {
		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_pdu("$editA:example.com", 2000, Some("$msg1:example.com")),
			make_pdu("$editB:example.com", 2000, Some("$msg1:example.com")),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 2);
		assert_eq!(result[1].1.event_id.as_str(), "$editB:example.com");
	}

	#[test]
	fn collapse_prefers_timeline_order_over_timestamp() {
		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_pdu_with_count_and_sender(
				"$edit_old_ts_newer_count:example.com",
				3000,
				1000,
				Some("$msg1:example.com"),
				"@user:example.com",
			),
			make_pdu_with_count_and_sender(
				"$edit_new_ts_older_count:example.com",
				2000,
				4000,
				Some("$msg1:example.com"),
				"@user:example.com",
			),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 2);
		assert_eq!(result[1].1.event_id.as_str(), "$edit_old_ts_newer_count:example.com");
	}

	#[test]
	fn collapse_groups_edits_by_target_and_sender() {
		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_pdu_with_sender(
				"$edit_a1:example.com",
				2000,
				Some("$msg1:example.com"),
				"@alice:example.com",
			),
			make_pdu_with_sender(
				"$edit_a2:example.com",
				3000,
				Some("$msg1:example.com"),
				"@alice:example.com",
			),
			make_pdu_with_sender(
				"$edit_b1:example.com",
				4000,
				Some("$msg1:example.com"),
				"@bob:example.com",
			),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 3);

		let event_ids: Vec<&str> = result
			.iter()
			.map(|(_, pdu)| pdu.event_id.as_str())
			.collect();
		assert!(event_ids.contains(&"$msg1:example.com"));
		assert!(event_ids.contains(&"$edit_a2:example.com"));
		assert!(event_ids.contains(&"$edit_b1:example.com"));
		assert!(!event_ids.contains(&"$edit_a1:example.com"));
	}

	#[test]
	fn collapse_non_replace_relations_untouched() {
		let make_reply = |event_id: &str, ts: u64, target: &str| -> (PduCount, PduEvent) {
			let content = format!(
				r#"{{"body":"reply","m.relates_to":{{"rel_type":"m.thread","event_id":"{target}"}}}}"#
			);
			let pdu = PduEvent {
				kind: ruma::events::TimelineEventType::RoomMessage,
				content: RawValue::from_string(content)
					.expect("valid JSON")
					.into(),
				event_id: EventId::parse(event_id).expect("valid event_id"),
				room_id: OwnedRoomId::try_from("!test:example.com").expect("valid room_id"),
				sender: OwnedUserId::try_from("@user:example.com").expect("valid user_id"),
				state_key: None,
				redacts: None,
				prev_events: Default::default(),
				auth_events: Default::default(),
				origin_server_ts: UInt::try_from(ts).expect("valid ts"),
				depth: UInt::try_from(1_u64).expect("valid depth"),
				hashes: EventHash::default(),
				origin: None,
				unsigned: None,
			};
			(PduCount::Normal(ts), pdu)
		};

		let events = vec![
			make_pdu("$msg1:example.com", 1000, None),
			make_reply("$thread1:example.com", 2000, "$msg1:example.com"),
			make_reply("$thread2:example.com", 3000, "$msg1:example.com"),
		];

		let result = collapse_superseded_edits(events);
		assert_eq!(result.len(), 3);
	}
}
