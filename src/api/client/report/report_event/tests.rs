use ruma::{event_id, room_alias_id, room_id, user_id};

use super::{BODY_MAX_CHARS, EventReport, format_report, quote_body};

fn report() -> EventReport<'static> {
	EventReport {
		reporter: user_id!("@reporter:example.org"),
		reason: "spam",
		event_id: event_id!("$event:example.org"),
		room_id: room_id!("!room:example.org"),
		room_type: "regular",
		room_alias: None,
		body: Some("hello"),
		sender: user_id!("@sender:example.org"),
		sender_is_local: false,
	}
}

#[test]
fn report_shows_the_new_details() {
	let text = format_report(&EventReport {
		room_type: "m.space",
		room_alias: Some(room_alias_id!("#room:example.org")),
		..report()
	});

	assert!(text.contains("Room Type: m.space"));
	assert!(text.contains("Room Alias: #room:example.org"));
	assert!(text.contains("Message: hello"));
}

#[test]
fn report_without_alias_or_body_says_so() {
	let text = format_report(&EventReport { body: None, ..report() });

	assert!(text.contains("Room Alias: none"));
	assert!(text.contains("Message: unavailable"));
}

#[test]
fn redact_hint_is_only_for_local_senders() {
	let local = format_report(&EventReport { sender_is_local: true, ..report() });
	let remote = format_report(&report());

	assert!(local.contains("!admin users redact-event $event:example.org"));
	assert!(!remote.contains("redact-event"));
}

#[test]
fn long_non_ascii_body_is_cut_without_panic() {
	let body = "я".repeat(BODY_MAX_CHARS + 1);

	assert_eq!(quote_body(&body).chars().count(), BODY_MAX_CHARS);
}

#[test]
fn body_cannot_inject_markup_or_lines() {
	let quoted = quote_body("<b>x</b> [link](https://example.org)\nSent By: @fake:example.org");

	assert!(!quoted.contains('<'));
	assert!(quoted.contains("\\[link]"));
	assert!(!quoted.contains('\n'));
}
