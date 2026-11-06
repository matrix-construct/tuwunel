use ruma::{
	RoomAliasId, event_id, events::room::message::RoomMessageEventContent, room_alias_id,
	room_id, user_id,
};
use serde_json::{to_value, to_vec};

use super::{DETAIL_MAX_CHARS, REASON_MAX_LEN, Report, excerpt, format_report};

#[test]
fn report_shows_the_new_details() {
	let text = format_report(&Report {
		room_type: "m.space",
		room_alias: Some(room_alias_id!("#room:example.org")),
		..report()
	});

	let body = text.body();

	assert!(body.contains("Room Type: m.space"));
	assert!(body.contains("Room Alias: #room:example.org"));
	assert!(body.contains("Message: hello"));
}

#[test]
fn report_without_alias_or_body_says_so() {
	let text = format_report(&Report { body: None, ..report() });
	let body = text.body();

	assert!(body.contains("Room Alias: none"));
	assert!(body.contains("Message: unavailable"));
}

#[test]
fn redact_hint_is_only_for_local_senders() {
	let local = format_report(&Report { sender_is_local: true, ..report() });
	let remote = format_report(&report());

	assert!(
		local
			.body()
			.contains("!admin users redact-event $event:example.org")
	);

	assert!(!remote.body().contains("redact-event"));
}

#[test]
fn long_non_ascii_body_is_cut_without_panic() {
	let body = "я".repeat(DETAIL_MAX_CHARS + 1);

	assert_eq!(excerpt(&body).chars().count(), DETAIL_MAX_CHARS);
}

#[test]
fn report_preserves_literal_text_in_both_bodies() {
	let input = "**bold** ~~gone~~ <b>x</b> [link](https://example.org)\nSent By: fake";
	let content = format_report(&Report {
		room_type: input,
		body: Some(input),
		..report()
	});

	let body = content.body();
	let html = formatted_body(&content);

	assert!(
		body.contains("**bold** ~~gone~~ <b>x</b> [link](https://example.org) Sent By: fake")
	);

	assert!(html.contains(
		"**bold** ~~gone~~ &lt;b&gt;x&lt;/b&gt; [link](https://example.org) Sent By: fake"
	));

	assert!(!html.contains("<a "));
	assert!(!html.contains("<strong>"));
	assert!(!html.contains("<del>"));
	assert_eq!(body.matches("\nSent By:").count(), 1);
}

#[test]
fn alias_cannot_inject_markup_or_lines() {
	let alias = room_alias_id!("#room\n\n[Open](//example.org)\n\n:example.org");
	let content = format_report(&Report { room_alias: Some(alias), ..report() });
	let body = content.body();
	let html = formatted_body(&content);

	assert!(body.contains("Room Alias: #room  [Open](//example.org)  :example.org"));

	assert!(html.contains("Room Alias: #room  [Open](//example.org)  :example.org"));
	assert!(!html.contains("<a "));
	assert_eq!(body.matches("\nSent By:").count(), 1);
}

#[test]
fn long_details_leave_room_for_the_event_envelope() {
	let room_type = "'".repeat(40_000);
	let body = "<".repeat(40_000);
	let reason = "'".repeat(REASON_MAX_LEN);
	let alias = format!("#{}:example.org", "'".repeat(40_000));
	let alias = RoomAliasId::parse(alias).expect("compatibility permits long aliases");
	let content = format_report(&Report {
		room_type: &room_type,
		room_alias: Some(&alias),
		body: Some(&body),
		reason: &reason,
		sender_is_local: true,
		..report()
	});

	let encoded = to_vec(&content).expect("serialize notice");
	let body = content.body();

	assert!(encoded.len() < 32_768);
	assert!(body.contains(&format!("Room Type: {}\n", "'".repeat(DETAIL_MAX_CHARS))));
	assert!(body.contains(&format!("Message: {}\n", "<".repeat(DETAIL_MAX_CHARS))));
}

#[test]
fn excerpt_preserves_short_text_and_character_boundaries() {
	for count in [DETAIL_MAX_CHARS - 1, DETAIL_MAX_CHARS, DETAIL_MAX_CHARS + 1] {
		let input = "\u{10400}".repeat(count);
		let expected = "\u{10400}".repeat(count.min(DETAIL_MAX_CHARS));

		assert_eq!(excerpt(&input), expected);
	}
}

fn report() -> Report<'static> {
	Report {
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

fn formatted_body(content: &RoomMessageEventContent) -> String {
	to_value(content).expect("serialize notice")["formatted_body"]
		.as_str()
		.expect("formatted body")
		.to_owned()
}
