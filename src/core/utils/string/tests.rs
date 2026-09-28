#![cfg(test)]

use std::borrow::Cow;

use super::{markdown_cell, plural};
use crate::{arrayvec::ArrayString, smallstr::SmallString};

type SmallBuf = SmallString<[u8; 16]>;
type ArrayBuf = ArrayString<16>;

#[test]
fn common_prefix() {
	let input = ["conduwuit", "conduit", "construct"];
	let output = super::common_prefix(&input);
	assert_eq!(output, "con");
}

#[test]
fn common_prefix_empty() {
	let input = ["abcdefg", "hijklmn", "opqrstu"];
	let output = super::common_prefix(&input);
	assert_eq!(output, "");
}

#[test]
fn common_prefix_none() {
	let input: [&str; 0] = [];
	let output = super::common_prefix(&input);
	assert_eq!(output, "");
}

#[test]
fn camel_to_snake_case_0() {
	let res = super::camel_to_snake_string("CamelToSnakeCase");
	assert_eq!(res, "camel_to_snake_case");
}

#[test]
fn camel_to_snake_case_1() {
	let res = super::camel_to_snake_string("CAmelTOSnakeCase");
	assert_eq!(res, "camel_tosnake_case");
}

#[test]
fn format_small_string_macro() {
	let res: SmallBuf = crate::format_small_string!(":{}", 8448);

	assert_eq!(res.as_str(), ":8448");
	assert!(!res.spilled());
}

#[test]
fn format_array_string_macro() {
	let res: ArrayBuf = crate::format_array_string!(":{}", 8448);

	assert_eq!(res.as_str(), ":8448");
}

#[test]
fn to_array_string_display() {
	let res: ArrayBuf = super::to_array_string(8448);

	assert_eq!(res.as_str(), "8448");
}

#[test]
fn try_format_array_string_overflows() {
	use crate::Error;

	let res = super::try_format_array_string::<4>(format_args!(":{}", 8448));

	assert!(matches!(res, Err(Error::Fmt(_))));
}

#[test]
fn unquote() {
	use super::Unquote;

	assert_eq!("\"foo\"".unquote(), Some("foo"));
	assert_eq!("\"foo".unquote(), None);
	assert_eq!("foo".unquote(), None);
}

#[test]
fn unquote_infallible() {
	use super::Unquote;

	assert_eq!("\"foo\"".unquote_infallible(), "foo");
	assert_eq!("\"foo".unquote_infallible(), "\"foo");
	assert_eq!("foo".unquote_infallible(), "foo");
}

#[test]
fn between() {
	use super::Between;

	assert_eq!("\"foo\"".between(("\"", "\"")), Some("foo"));
	assert_eq!("\"foo".between(("\"", "\"")), None);
	assert_eq!("foo".between(("\"", "\"")), None);
}

#[test]
fn between_infallible() {
	use super::Between;

	assert_eq!("\"foo\"".between_infallible(("\"", "\"")), "foo");
	assert_eq!("\"foo".between_infallible(("\"", "\"")), "\"foo");
	assert_eq!("foo".between_infallible(("\"", "\"")), "foo");
}

/// Collects the chunks of `text` under a raw-byte budget.
fn chunks(text: &str, markdown: bool, budget: usize) -> Vec<String> {
	super::chunk(text, markdown, move |s: &str| s.len() <= budget).collect()
}

#[test]
fn chunk_identity_when_it_fits() {
	let text = "one\ntwo\nthree";

	assert_eq!(chunks(text, false, 1024), vec![text.to_owned()]);
	assert_eq!(chunks(text, true, 1024), vec![text.to_owned()]);
}

#[test]
fn chunk_empty_input_yields_one_empty_segment() {
	assert_eq!(chunks("", true, 16), vec![String::new()]);
	assert_eq!(chunks("", false, 16), vec![String::new()]);
}

#[test]
fn chunk_plain_split_reconstructs_exactly() {
	let text = "alpha\nbravo\ncharlie\ndelta\necho\n";
	let parts = chunks(text, false, 12);

	assert!(parts.len() > 1);
	assert!(parts.iter().all(|p| p.len() <= 12));
	assert_eq!(parts.concat(), text);
}

#[test]
fn chunk_plain_mode_never_injects_fences() {
	let text = "```rust\nlet x = 1;\nlet y = 2;\n```\ntail\n";
	let parts = chunks(text, false, 14);

	assert!(parts.len() > 1);
	assert_eq!(parts.concat(), text);
}

#[test]
fn chunk_markdown_fence_closes_and_reopens_with_info() {
	let text = "```rust\nline1\nline2\n```\n";
	let parts = chunks(text, true, 20);

	assert_eq!(parts, vec![
		"```rust\nline1\n```\n".to_owned(),
		"```rust\nline2\n```\n".to_owned(),
	]);

	assert!(parts.iter().all(|p| p.len() <= 20));
}

#[test]
fn chunk_markdown_reconstructs_after_stripping_fence_lines() {
	let text = "```py\na\nb\nc\nd\n```\n";
	let parts = chunks(text, true, 12);
	let strip = |s: &str| {
		s.lines()
			.filter(|l| !l.trim_start().starts_with("```"))
			.collect::<Vec<_>>()
			.join("\n")
	};

	assert!(parts.iter().all(|p| p.len() <= 12));
	assert_eq!(strip(&parts.concat()), strip(text));
}

#[test]
fn chunk_long_line_hard_splits_within_budget() {
	let text = "x".repeat(50);
	let parts = chunks(&text, false, 16);

	assert!(parts.len() >= 3);
	assert!(parts.iter().all(|p| p.len() <= 16));
	assert_eq!(parts.concat(), text);
}

#[test]
fn chunk_long_line_splits_on_char_boundaries() {
	let text = "λ".repeat(20); // each λ is two bytes
	let parts = chunks(&text, false, 9);

	assert!(parts.len() > 1);
	assert!(parts.iter().all(|p| p.len() <= 9));
	assert_eq!(parts.concat(), text);
}

#[test]
fn chunk_every_segment_honors_the_budget() {
	let text = "# header\n\n```json\n{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n```\n\nfinal \
	            paragraph with a fairly long single line of prose here\n";

	let parts = chunks(text, true, 24);

	assert!(parts.len() > 1);
	assert!(
		parts
			.iter()
			.all(|p| !p.is_empty() && p.len() <= 24)
	);
}

#[test]
fn chunk_segment_count_tracks_the_budget() {
	let text = "aaaa\nbbbb\ncccc\ndddd\n"; // four 5-byte lines

	assert_eq!(chunks(text, false, 4096).len(), 1);
	assert_eq!(chunks(text, false, 10).len(), 2);
	assert_eq!(chunks(text, false, 5).len(), 4);
}

#[test]
fn markdown_cell_escapes_what_would_break_the_cell() {
	assert_eq!(markdown_cell("a|b\\c\nd\u{2028}e"), "a\\|b\\\\c d e");
}

#[test]
fn markdown_cell_borrows_a_clean_value() {
	let clean = "!room:example.org";

	assert!(matches!(markdown_cell(clean), Cow::Borrowed(borrowed) if borrowed == clean));
}

#[test]
fn plural_takes_the_singular_for_one_only() {
	let nouns = [0, 1, 2].map(|count| plural(count, "room", "rooms"));

	assert_eq!(nouns, ["rooms", "room", "rooms"]);
}
