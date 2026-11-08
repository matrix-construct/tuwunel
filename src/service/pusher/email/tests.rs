use std::time::Duration;

use ruma::{UserId, owned_room_id, user_id};
use serde_json::{Value, json};
use tuwunel_core::utils::BoolExt;
use tuwunel_matrix::pdu::Pdu;

use super::{
	Outcome, Schedule, entry_in,
	evaluate::{Pending, Scan, Throttle, due_at, effective_throttle, gather, sent_throttle},
	render::{Digest, Line, Room, SHOWN, snippet, subject, text},
	reschedule, woken,
};

const DELAY: u64 = 600_000;
const DAY: u64 = 24 * 60 * 60 * 1000;

/// A room's throttle starts at the delay, then each mail multiplies it by 144,
/// never past a day.
///
/// The default ten minute delay times 144 is exactly a day, so the second mail
/// already reaches the cap. Without a delay there is no throttle at all.
#[test]
fn throttle_grows_to_a_day() {
	let first = sent_throttle(Throttle::default(), 1, 7, DELAY);
	let second = sent_throttle(first, 2, 8, DELAY);
	let third = sent_throttle(second, 3, 9, DELAY);

	assert_eq!(first.period_ms, DELAY);
	assert_eq!(second.period_ms, DAY);
	assert_eq!(third.period_ms, DAY);
	assert_eq!((third.last_sent_ms, third.sent_count), (3, 9));
	assert_eq!(sent_throttle(third, 4, 10, 0).period_ms, 0);
	assert_eq!(sent_throttle(Throttle::default(), 5, 11, 2 * DAY).period_ms, DAY);
}

/// Reading the room after the last mail resets its throttle.
///
/// A read at or before the mail's count leaves it unchanged, and the next mail
/// after a reset starts again at the delay.
#[test]
fn throttle_resets_on_read() {
	let stored = Throttle {
		last_sent_ms: 1000,
		period_ms: DAY,
		sent_count: 50,
	};

	let read = effective_throttle(stored, Some(51));

	assert_eq!(read, Throttle { period_ms: 0, ..stored });
	assert_eq!(effective_throttle(stored, Some(50)), stored);
	assert_eq!(effective_throttle(stored, None), stored);
	assert_eq!(due_at(5_000, read, DELAY), 5_000 + DELAY);
	assert_eq!(sent_throttle(read, 2000, 60, DELAY).period_ms, DELAY);
}

#[test]
fn due_after_delay_and_throttle() {
	let throttle = Throttle {
		last_sent_ms: 10_000,
		period_ms: DAY,
		sent_count: 0,
	};

	assert_eq!(due_at(5_000, Throttle::default(), DELAY), 5_000 + DELAY);
	assert_eq!(due_at(5_000, throttle, DELAY), 10_000 + DAY);
	assert_eq!(due_at(5_000, throttle, 0), 5_000);
}

/// A scan keeps only the newest `SHOWN` notifications to mail, and each room's
/// earliest arrival, however many rows it reads.
///
/// Every third row has nothing to mail, yet the last of them, row 51, still
/// ends the scan. Odd rows go to room 1 and even rows to room 0, each timed at
/// ten times its row.
#[test]
fn scan_keeps_newest_and_each_rooms_earliest() {
	let content = json!({ "msgtype": "m.text", "body": "hi" });
	let pending = |count: u64| Pending {
		ts: count * 10,
		sroomid: count % 2,
		pdu: event("m.room.message", None, &content),
	};

	let scan = (1_u64..=51)
		.map(|count| (count, count.is_multiple_of(3).or(|| pending(count))))
		.fold(Scan::default(), gather);

	assert_eq!((scan.first, scan.last, scan.total), (Some(1), Some(51), 34));
	assert_eq!(scan.newest.len(), SHOWN);
	assert_eq!(scan.newest.back().map(|newest| newest.ts), Some(500));
	assert_eq!(scan.rooms[&1].1, 10);
	assert_eq!(scan.rooms[&0].1, 20);
}

#[test]
fn encrypted_snippet_hides_ciphertext() {
	let content = json!({
		"algorithm": "m.megolm.v1.aes-sha2",
		"ciphertext": "SECRETCIPHERTEXT",
		"session_id": "session",
		"device_id": "DEVICE",
		"sender_key": "key",
	});

	let snippet = snippet(&event("m.room.encrypted", None, &content), user());

	assert_eq!(snippet, "Encrypted message");
}

#[test]
fn message_snippet_is_plain_and_cut() {
	let long = "word ".repeat(100);
	let content = json!({
		"msgtype": "m.text",
		"body": format!("first\nsecond {long}"),
		"format": "org.matrix.custom.html",
		"formatted_body": "<script>FORMATTED</script>",
	});

	let snippet = snippet(&event("m.room.message", None, &content), user());

	assert!(snippet.starts_with("first second word"), "{snippet}");
	assert!(!snippet.contains("FORMATTED"), "{snippet}");
	assert_eq!(snippet.chars().count(), 300);
}

#[test]
fn invite_snippet() {
	let user = user();
	let content = json!({ "membership": "invite" });
	let invite = event("m.room.member", Some(user.as_str()), &content);

	assert_eq!(snippet(&invite, user), "invited you");
}

/// The subject names the most recent room and counts the others, including a
/// room whose notifications are all left out.
///
/// The text counts the notifications left out, and a lone invite's subject
/// names the inviter instead, and the room only when it has a name of its own.
#[test]
fn subject_counts_other_rooms() {
	let room = |name: &str| Room {
		name: Some(name.to_owned()),
		id: owned_room_id!("!room:example.com"),
		lines: Vec::new(),
	};

	let digest = Digest {
		brand: "Brand",
		address: "recipient@example.com",
		rooms: vec![room("Newest"), room("Older"), room("Oldest")],
		hidden: 3,
		room_count: 4,
		invite: false,
	};

	assert_eq!(subject(&digest), "[Brand] New messages in Newest and 3 other rooms");
	assert!(text(&digest).contains("And 3 more notifications."));

	let line = Line {
		sender: "Alice".to_owned(),
		time: String::new(),
		snippet: "invited you".into(),
	};

	let party = Room { lines: vec![line], ..room("Party") };
	let invite = Digest {
		invite: true,
		rooms: vec![party],
		room_count: 1,
		..digest
	};

	assert_eq!(subject(&invite), "[Brand] Alice invited you to Party");

	let unnamed = Digest {
		rooms: vec![Room {
			name: None,
			..invite.rooms.into_iter().next().unwrap()
		}],
		..invite
	};

	assert_eq!(subject(&unnamed), "[Brand] Alice invited you to a room");
	assert!(text(&unnamed).starts_with("You have unread notifications on Brand.\n\nAlice\n"));
}

/// A cursor start that failed keeps its frontier through the backoff it
/// causes, until a wake that starts the cursor clears it.
///
/// The clearing wake leaves the backoff alone, and a sent outcome drops the
/// frontier with the entry.
#[test]
fn failed_start_keeps_its_frontier() {
	let key = (user().to_owned(), "mail@example.com".into());
	let schedule = reschedule(Schedule::new(), key.clone(), Outcome::Failed, Some(7));
	let failed = schedule[&key];

	assert_eq!((failed.frontier, failed.failures), (Some(7), 1));

	let schedule = woken(schedule, key.clone(), entry_in(Duration::ZERO, 0));
	let started = schedule[&key];

	assert_eq!((started.frontier, started.failures, started.due), (None, 1, failed.due));
	assert!(reschedule(schedule, key, Outcome::Sent, None).is_empty());
}

fn event(kind: &str, state_key: Option<&str>, content: &Value) -> Pdu {
	serde_json::from_value(json!({
		"type": kind,
		"state_key": state_key,
		"content": content,
		"event_id": "$event:example.com",
		"room_id": "!room:example.com",
		"sender": "@sender:example.com",
		"prev_events": ["$prev:example.com"],
		"auth_events": ["$auth:example.com"],
		"origin_server_ts": 1_838_188_000,
		"depth": 12,
		"hashes": { "sha256": "thishashcoversallfieldsincasethisisredacted" },
	}))
	.unwrap()
}

fn user() -> &'static UserId { user_id!("@recipient:example.com") }
