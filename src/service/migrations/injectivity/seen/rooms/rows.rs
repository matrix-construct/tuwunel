use std::str::from_utf8;

use ruma::{EventId, ServerName, UserId};
use serde::Deserialize;
use serde_json::{Value as JsonValue, from_slice};
use tuwunel_core::utils::BoolExt;
use tuwunel_database::{Json, SEP, serialize_val};

use self::Part::{Key, Value};
use crate::pusher::Notified;

type Spot = Option<(Part, usize)>;

type Record = (Box<[u8]>, Box<[u8]>);

pub(super) const COLUMNS: [&str; 11] = [
	"pduid_pdu",
	"eventid_pduid",
	"tokenids",
	"relatesto_typed",
	"threadid_userids",
	"threadrootid_latestcount",
	"threadactivityid_rootid",
	"useridcount_notification",
	"servernameevent_data",
	"servercurrentevent_data",
	"servershortroomid_park",
];

// Keys in these columns lead with the short room id, so a prefix scan finds a room's rows.
pub(super) const SCANNED: [usize; 6] = [0, 2, 3, 4, 5, 6];

#[derive(Clone, Copy)]
pub(super) struct Decoded {
	pub(super) rooms: [u64; 2],
	spots: [Spot; 2],
}

#[derive(Clone, Copy)]
enum Part {
	Key,
	Value,
}

#[derive(Deserialize)]
struct NotifiedRoom {
	sroomid: u64,
}

pub(super) fn decode(column: usize, key: &[u8], value: &[u8]) -> Option<Decoded> {
	let spots = match column {
		| 0 => {
			raw(key)?;
			[Some((Key, 0)), None]
		},
		| 1 => {
			<&EventId>::try_from(from_utf8(key).ok()?).ok()?;
			raw(value)?;
			[Some((Value, 0)), None]
		},
		| 2 => {
			let (token, end) = segment(key, 8)?;
			let start = end.saturating_add(1);

			token.is_empty().is_false().into_option()?;
			raw(key.get(start..)?)?;
			value.is_empty().into_option()?;
			[Some((Key, 0)), Some((Key, start))]
		},
		| 3 => {
			let valid = key.len() == 33 && matches!(key[16], 1 | 2);

			valid.into_option()?;
			let room = word(&key[..8])?;

			normal(room, word(&key[8..16])?)?;
			normal(room, word(&key[25..])?)?;
			short(value)?;
			[Some((Key, 0)), None]
		},
		| 4 => {
			raw(key)?;
			users(value)?;
			[Some((Key, 0)), None]
		},
		| 5 => {
			normal(raw(key)?, word(value)?)?;
			[Some((Key, 0)), None]
		},
		| 6 => {
			raw(key)?;
			raw(value)?;
			[Some((Key, 0)), Some((Value, 0))]
		},
		| 7 => {
			let (user, end) = segment(key, 0)?;

			<&UserId>::try_from(user).ok()?;
			let count = word(key.get(end.saturating_add(1)..)?)?;
			let room = from_slice(value)
				.ok()
				.map(|stored: Notified| stored.sroomid)?;

			normal(room, count)?;
			return Some(Decoded { rooms: [room, 0], spots: [None; 2] });
		},
		| 8 | 9 if value.is_empty().is_false() =>
			return Some(Decoded { rooms: [0; 2], spots: [None; 2] }),
		| 8 | 9 => {
			let end = destination(key)?;

			raw(key.get(end..)?)?;
			[Some((Key, end)), None]
		},
		| 10 => {
			let (server, end) = segment(key, 0)?;

			<&ServerName>::try_from(server).ok()?;
			let valid =
				value.len() == 17 && value[8] == SEP && key.len() == end.saturating_add(9);

			valid.into_option()?;
			[Some((Key, end.saturating_add(1))), None]
		},
		| _ => return None,
	};

	let rooms = spots.map(|spot| {
		spot.map_or(Some(0), |(part, offset)| short_at(part.select(key, value), offset))
	});

	Some(Decoded { rooms: [rooms[0]?, rooms[1]?], spots })
}

pub(super) fn hints(column: usize, key: &[u8], value: &[u8]) -> [Option<u64>; 2] {
	match column {
		| 1 => [short_at(value, 0), None],
		| 2 => {
			let pdu_room = separator(key, 8).and_then(|end| short_at(key, end.saturating_add(1)));

			[short_at(key, 0), pdu_room]
		},
		| 6 => [short_at(key, 0), short_at(value, 0)],
		| 7 => [sroomid(value), None],
		| 8 | 9 => [destination(key).and_then(|end| short_at(key, end)), None],
		| 10 => {
			let room = key
				.len()
				.checked_sub(8)
				.and_then(|offset| short_at(key, offset));

			[room, None]
		},
		| _ => [short_at(key, 0), None],
	}
}

pub(super) fn rewrite(
	decoded: Decoded,
	column: usize,
	key: &[u8],
	value: &[u8],
	from: u64,
	to: u64,
) -> Option<Record> {
	let value = if column == 7 {
		notification(value, to)?
	} else {
		Box::from(value)
	};

	Some(patch(Box::from(key), value, decoded, from, to))
}

impl Part {
	fn select<T>(self, key: T, value: T) -> T {
		match self {
			| Key => key,
			| Value => value,
		}
	}
}

pub(super) fn raw(bytes: &[u8]) -> Option<u64> {
	let room = short_at(bytes, 0)?;
	let count = i64::from_be_bytes(*bytes.last_chunk()?);
	let valid = match bytes.len() {
		| 16 => count > 0,
		| 24 => bytes[8..16] == [0; 8] && count <= 0,
		| _ => false,
	};

	valid.then_some(room)
}

pub(super) fn word(bytes: &[u8]) -> Option<u64> { bytes.try_into().ok().map(u64::from_be_bytes) }

fn segment(key: &[u8], start: usize) -> Option<(&str, usize)> {
	let end = separator(key, start)?;

	from_utf8(&key[start..end])
		.ok()
		.map(|text| (text, end))
}

fn separator(key: &[u8], start: usize) -> Option<usize> {
	key.get(start..)?
		.iter()
		.position(|byte| *byte == SEP)
		.map(|offset| offset.saturating_add(start))
}

fn normal(room: u64, count: u64) -> Option<()> {
	let valid = room != 0 && count > 0 && count <= i64::MAX.cast_unsigned();

	valid.into_option()
}

fn users(value: &[u8]) -> Option<()> {
	value
		.split(|byte| *byte == SEP)
		.try_for_each(|user| {
			from_utf8(user)
				.ok()
				.and_then(|user| <&UserId>::try_from(user).ok())
				.map(|_| ())
		})
}

fn destination(key: &[u8]) -> Option<usize> {
	let (prefix, end) = segment(key, 0)?;
	let after = end.saturating_add(1);

	match key.first()? {
		| b'$' => prefix
			.get(1..)
			.and_then(|user| <&UserId>::try_from(user).ok())
			.and_then(|_| segment(key, after))
			.map(|(_, next)| next.saturating_add(1)),
		| b'+' => prefix.len().gt(&1).then_some(after),
		| _ => <&ServerName>::try_from(prefix)
			.ok()
			.map(|_| after),
	}
}

fn short_at(bytes: &[u8], offset: usize) -> Option<u64> {
	bytes
		.get(offset..offset.saturating_add(8))
		.and_then(short)
}

fn short(bytes: &[u8]) -> Option<u64> { word(bytes).filter(|id| id.ne(&0)) }

fn sroomid(value: &[u8]) -> Option<u64> {
	// None leaves the row's room unknown, and a zero sroomid names no room.
	from_slice(value)
		.ok()
		.map(|room: NotifiedRoom| room.sroomid)
		.filter(|id| id.ne(&0))
}

fn notification(value: &[u8], to: u64) -> Option<Box<[u8]>> {
	// Untyped, so fields this build does not model survive the rewrite.
	from_slice(value)
		.ok()
		.and_then(|json| notified(json, to))
		.and_then(|json| serialize_val(Json(&json)).ok())
		.map(|bytes| Box::from(&*bytes))
}

fn notified(mut json: JsonValue, to: u64) -> Option<JsonValue> {
	*json.get_mut("sroomid")? = JsonValue::from(to);

	Some(json)
}

fn patch(
	mut key: Box<[u8]>,
	mut value: Box<[u8]>,
	decoded: Decoded,
	from: u64,
	to: u64,
) -> Record {
	decoded
		.spots
		.into_iter()
		.zip(decoded.rooms)
		.filter(|(_, room)| room.eq(&from))
		.filter_map(|(spot, _)| spot)
		.for_each(|(part, offset)| {
			let bytes = part.select(&mut *key, &mut *value);

			bytes[offset..offset.saturating_add(8)].copy_from_slice(&to.to_be_bytes());
		});

	(key, value)
}
