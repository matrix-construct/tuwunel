use std::mem::take;

use ruma::{
	RoomId, UserId,
	api::client::filter::{Filter, RoomEventFilter, RoomFilter, UrlFilter},
	serde::Raw,
};
use serde_json::{Map, Value};
use smallvec::SmallVec;

use super::Event;
use crate::is_equal_to;

/// Segments of one `event_fields` entry; most paths are one or two deep.
type FieldPath = SmallVec<[String; 2]>;

/// Tests values against a Matrix filter.
///
/// Implementations apply the allow and deny lists relevant to the value being
/// tested. A value matches only when every applicable filter condition accepts
/// it.
pub trait Matches<T> {
	/// Returns whether the supplied value satisfies this filter.
	///
	/// The value type selects which filter fields participate in the
	/// comparison. Built-in implementations reject deny-list matches before
	/// accepting allow-list matches.
	fn matches(&self, t: T) -> bool;
}

impl<E: Event> Matches<&E> for RoomEventFilter {
	#[inline]
	fn matches(&self, event: &E) -> bool {
		if !matches_sender(event, self) {
			return false;
		}

		if !matches_room(event, self) {
			return false;
		}

		if !matches_type(event, self) {
			return false;
		}

		if !matches_url(event, self) {
			return false;
		}

		true
	}
}

impl Matches<&RoomId> for RoomFilter {
	#[inline]
	fn matches(&self, room_id: &RoomId) -> bool {
		if !matches_room_id(room_id, self) {
			return false;
		}

		true
	}
}

impl Matches<&UserId> for Filter {
	#[inline]
	fn matches(&self, user_id: &UserId) -> bool {
		if !matches_user_id(user_id, self) {
			return false;
		}

		true
	}
}

fn matches_user_id(user_id: &UserId, filter: &Filter) -> bool {
	if filter
		.not_senders
		.iter()
		.any(is_equal_to!(user_id))
	{
		return false;
	}

	if let Some(senders) = filter.senders.as_ref()
		&& !senders.iter().any(is_equal_to!(user_id))
	{
		return false;
	}

	true
}

fn matches_room_id(room_id: &RoomId, filter: &RoomFilter) -> bool {
	if filter.not_rooms.iter().any(is_equal_to!(room_id)) {
		return false;
	}

	if let Some(rooms) = filter.rooms.as_ref()
		&& !rooms.iter().any(is_equal_to!(room_id))
	{
		return false;
	}

	true
}

fn matches_room<E: Event>(event: &E, filter: &RoomEventFilter) -> bool {
	if filter
		.not_rooms
		.iter()
		.any(is_equal_to!(event.room_id()))
	{
		return false;
	}

	if let Some(rooms) = filter.rooms.as_ref()
		&& !rooms.iter().any(is_equal_to!(event.room_id()))
	{
		return false;
	}

	true
}

fn matches_sender<E: Event>(event: &E, filter: &RoomEventFilter) -> bool {
	if filter
		.not_senders
		.iter()
		.any(is_equal_to!(event.sender()))
	{
		return false;
	}

	if let Some(senders) = filter.senders.as_ref()
		&& !senders.iter().any(is_equal_to!(event.sender()))
	{
		return false;
	}

	true
}

fn matches_type<E: Event>(event: &E, filter: &RoomEventFilter) -> bool {
	let kind = event.kind().to_cow_str();

	if filter.not_types.iter().any(is_equal_to!(&kind)) {
		return false;
	}

	if let Some(types) = filter.types.as_ref()
		&& !types.iter().any(is_equal_to!(&kind))
	{
		return false;
	}

	true
}

fn matches_url<E: Event>(event: &E, filter: &RoomEventFilter) -> bool {
	let Some(url_filter) = filter.url_filter.as_ref() else {
		return true;
	};

	//TODO: might be better to use Ruma's Raw rather than serde here
	let url = event
		.get_content_as_value()
		.get("url")
		.is_some_and(Value::is_string);

	match url_filter {
		| UrlFilter::EventsWithUrl => url,
		| UrlFilter::EventsWithoutUrl => !url,
	}
}

/// Restrict a serialized event to the requested `event_fields` per MSC3980,
/// returning it unchanged when no filter is set so the common sync pays
/// nothing.
///
/// The deserialize and reserialize happen only when `event_fields` is `Some`.
#[must_use]
pub fn trim_event_fields<T>(raw: Raw<T>, event_fields: Option<&[String]>) -> Raw<T> {
	// Identity keys clients rely on are always kept; the spec permits a superset.
	const ALWAYS_KEEP: [&str; 6] =
		["event_id", "type", "sender", "origin_server_ts", "state_key", "room_id"];

	let Some(fields) = event_fields else {
		return raw;
	};

	let Ok(Value::Object(source)) = raw.deserialize_as::<Value>() else {
		return raw;
	};

	let mut out: Map<String, Value> = ALWAYS_KEEP
		.into_iter()
		.filter_map(|key| {
			source
				.get(key)
				.map(|value| (key.to_owned(), value.clone()))
		})
		.collect();

	for field in fields {
		copy_field_path(&source, &mut out, &split_field(field));
	}

	Raw::from_json_value(&Value::Object(out))
}

/// Copy the leaf or subtree at `segments` from `source` into the same nested
/// path in `out`, creating intermediate objects as needed; an absent or
/// non-object path is skipped.
fn copy_field_path(
	source: &Map<String, Value>,
	out: &mut Map<String, Value>,
	segments: &[String],
) {
	let Some((head, rest)) = segments.split_first() else {
		return;
	};

	let Some(value) = source.get(head) else {
		return;
	};

	if rest.is_empty() {
		out.insert(head.clone(), value.clone());
		return;
	}

	let Value::Object(child) = value else {
		return;
	};

	if let Value::Object(child_out) = out
		.entry(head.clone())
		.or_insert_with(|| Value::Object(Map::new()))
	{
		copy_field_path(child, child_out, rest);
	}
}

/// Split one `event_fields` entry into its segments, inverting the MSC3873
/// escape grammar used by ruma's `escape_key`: `\.` is a literal dot inside the
/// current segment, `\\` a literal backslash, any other `\X` is kept verbatim,
/// and an unescaped `.` ends the segment.
fn split_field(field: &str) -> FieldPath {
	let mut segments = FieldPath::new();
	let mut current = String::new();
	let mut chars = field.chars();
	while let Some(c) = chars.next() {
		match c {
			| '\\' => match chars.next() {
				| Some('.') => current.push('.'),
				| Some('\\') | None => current.push('\\'),
				| Some(other) => {
					current.push('\\');
					current.push(other);
				},
			},
			| '.' => {
				segments.push(take(&mut current));
			},
			| other => current.push(other),
		}
	}

	segments.push(current);
	segments
}

#[cfg(test)]
mod tests {
	use super::split_field;

	fn segments(field: &str) -> Vec<String> { split_field(field).into_vec() }

	#[test]
	fn split_simple_path() {
		assert_eq!(segments("content.body"), ["content", "body"]);
	}

	#[test]
	fn split_escaped_dot() {
		assert_eq!(segments(r"content.m\.relates_to"), ["content", "m.relates_to"]);
	}

	#[test]
	fn split_escaped_backslash_then_dot() {
		assert_eq!(segments(r"a\\.b"), [r"a\", "b"]);
	}

	#[test]
	fn split_reserved_escape_kept_verbatim() {
		assert_eq!(segments(r"weird\x"), [r"weird\x"]);
	}

	#[test]
	fn split_trailing_backslash() {
		assert_eq!(segments(r"foo\"), [r"foo\"]);
	}
}
