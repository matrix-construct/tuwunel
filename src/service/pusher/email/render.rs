use std::{borrow::Cow, fmt::Write};

use futures::StreamExt;
use ruma::{OwnedRoomId, RoomId, UserId, events::TimelineEventType};
use serde::Deserialize;
use tuwunel_core::{
	Result, debug_warn, implement,
	itertools::Itertools,
	result::{FlatOk, NotFound},
	utils::{
		BoolExt,
		stream::{IterStream, WidebandExt},
		string::{plural, single_line},
		time::format as format_time,
	},
};
use tuwunel_matrix::{Event, pdu::Pdu};

use super::evaluate::{Pending, Scan, invites};
use crate::sendmail::Address;

pub(super) struct Mail {
	pub(super) subject: String,
	pub(super) text: String,
}

/// What a notification email says, with every name already resolved.
///
/// Rooms come most recent first, so the renderers only lay it out.
pub(super) struct Digest<'a> {
	pub(super) brand: &'a str,

	pub(super) address: &'a str,

	pub(super) rooms: Vec<Room>,

	/// Notifications left out past `SHOWN`.
	pub(super) hidden: usize,

	/// Rooms with notifications to mail, including any whose notifications are
	/// all left out.
	pub(super) room_count: usize,

	/// Whether the mail's only notification is an invite, whose sender the
	/// subject names.
	pub(super) invite: bool,
}

pub(super) struct Room {
	/// The room's name or canonical alias, when it has either.
	pub(super) name: Option<String>,

	pub(super) id: OwnedRoomId,

	pub(super) lines: Vec<Line>,
}

pub(super) struct Line {
	pub(super) sender: String,
	pub(super) time: String,
	pub(super) snippet: Cow<'static, str>,
}

#[derive(Deserialize)]
struct Body {
	body: String,
}

/// Notifications shown in one email.
///
/// The rest are counted at the end.
pub(super) const SHOWN: usize = 20;

const SNIPPET_CHARS: usize = 300;

const UNNAMED_ROOM: &str = "an unnamed room";

/// Renders the email for a scan's newest pending notifications.
///
/// Rooms come most recent first and each lists its notifications newest first;
/// the scan's remaining notifications are counted after them, and the subject
/// counts every room in the scan. A lone pending invite names its inviter in
/// the subject.
#[implement(super::super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%user_id,
	),
)]
pub(super) async fn render_email(
	&self,
	user_id: &UserId,
	address: &Address,
	brand: &str,
	scan: &Scan,
) -> Mail {
	let shown = &scan.newest;
	let newest = shown.iter().rev();
	let rooms: Vec<_> = newest
		.clone()
		.map(|pending| pending.pdu.room_id())
		.unique()
		.stream()
		.wide_then(async |room_id| {
			let notifications = newest
				.clone()
				.filter(|pending| pending.pdu.room_id() == room_id);

			self.render_room(user_id, room_id, notifications)
				.await
		})
		.collect()
		.await;

	let invite = shown
		.iter()
		.exactly_one()
		.is_ok_and(|only| invites(&only.pdu, user_id));

	let digest = Digest {
		brand,
		address: address.as_ref(),
		rooms,
		hidden: scan.total.saturating_sub(shown.len()),
		room_count: scan.rooms.len(),
		invite,
	};

	Mail {
		subject: subject(&digest),
		text: text(&digest),
	}
}

#[implement(super::super::Service)]
async fn render_room<'a, Shown>(&self, user_id: &UserId, room_id: &RoomId, shown: Shown) -> Room
where
	Shown: Iterator<Item = &'a Pending> + Send,
{
	// Sequential: the room fan-out in `render_email` is the parallelism budget.
	let lines: Vec<_> = shown
		.stream()
		.then(async |pending| {
			let sender = self
				.sender_name(room_id, pending.pdu.sender())
				.await;

			Line {
				sender,
				time: sent_at(&pending.pdu),
				snippet: snippet(&pending.pdu, user_id),
			}
		})
		.collect()
		.await;

	let name = self.room_name(room_id).await;

	Room { name, id: room_id.to_owned(), lines }
}

/// The sender's name in the room, else in their profile, else their ID.
///
/// Clients label a sender with the name set for the room, so the mail does too.
/// Each name is put on one line first, and one left with no text falls through
/// to the next.
#[implement(super::super::Service)]
async fn sender_name(&self, room_id: &RoomId, sender: &UserId) -> String {
	let member = self
		.services
		.state_accessor
		.get_member(room_id, sender)
		.await;

	let name = found(member, room_id, Some(sender))
		.and_then(|member| member.displayname)
		.and_then(visible_name);

	if let Some(name) = name {
		return name;
	}

	let displayname = self.services.profile.displayname(sender).await;

	found(displayname, room_id, Some(sender))
		.and_then(visible_name)
		.unwrap_or_else(|| one_line(sender.to_string()))
}

/// A name read's value, or `None` when it is absent or cannot be read.
///
/// A failed read is logged at debug level, since it costs the mail only a
/// label.
fn found<T>(read: Result<T>, room_id: &RoomId, sender: Option<&UserId>) -> Option<T> {
	read.optional()
		.inspect_err(|error| {
			// Debug: a remote's malformed member or name event fails here at every mail.
			debug_warn!(%room_id, ?sender, %error, "Failed to read a name for an email");
		})
		.flat_ok()
}

/// Remote-supplied text put on one line, or `None` when that leaves no text.
///
/// A name with nothing to show falls through to the next one.
fn visible_name(text: String) -> Option<String> {
	Some(one_line(text)).filter(|name| !name.trim().is_empty())
}

/// Remote-supplied text put on one line.
///
/// Names and message bodies come from remote users, so they could otherwise add
/// lines to the mail; text already on one line is returned as it is.
fn one_line(text: String) -> String {
	if let Cow::Owned(line) = single_line(&text) {
		line
	} else {
		text
	}
}

fn sent_at(pdu: &Pdu) -> String {
	pdu.origin_server_ts()
		.to_system_time()
		.map(|timepoint| format_time(timepoint, "%Y-%m-%d %H:%M UTC"))
		.unwrap_or_default()
}

/// What a notification says in the email.
///
/// A message shows its body on one line, cut at `SNIPPET_CHARS` characters,
/// and never its formatted body. An encrypted event shows only that it is one.
pub(super) fn snippet(pdu: &Pdu, user_id: &UserId) -> Cow<'static, str> {
	match pdu.kind() {
		| TimelineEventType::RoomEncrypted => "Encrypted message".into(),
		| TimelineEventType::RoomMember if invites(pdu, user_id) => "invited you".into(),
		| TimelineEventType::RoomMessage => pdu
			.get_content()
			.map(|Body { body }| shorten(body).into())
			.unwrap_or_else(|_| "sent a message".into()),
		| _ => "sent an event".into(),
	}
}

/// The body cut at `SNIPPET_CHARS` characters and put on one line.
///
/// The cut counts characters, so it never splits one.
fn shorten(mut body: String) -> String {
	if let Some((end, _)) = body.char_indices().nth(SNIPPET_CHARS) {
		body.truncate(end);
	}

	one_line(body)
}

/// The room's name, else its canonical alias, put on one line.
///
/// A name left with no text counts as none. A room with neither is `None`, and
/// the mail labels it by its first sender.
#[implement(super::super::Service)]
async fn room_name(&self, room_id: &RoomId) -> Option<String> {
	let state = &self.services.state_accessor;
	let name = state.get_name(room_id).await;
	let name = found(name, room_id, None).and_then(visible_name);

	if name.is_some() {
		return name;
	}

	let alias = state.get_canonical_alias(room_id).await;

	found(alias, room_id, None)
		.map(String::from)
		.and_then(visible_name)
}

/// The email's subject line.
///
/// It names the most recent room and counts the others, or names the inviter
/// of a lone invite, and the room when it has a name of its own.
pub(super) fn subject(digest: &Digest<'_>) -> String {
	let brand = digest.brand;
	let latest = digest.rooms.first();
	let name = latest.map_or(UNNAMED_ROOM, Room::label);
	let inviter = digest.invite.and_then(|| latest?.first_sender());

	match (inviter, digest.room_count) {
		| (None, 0 | 1) => format!("[{brand}] New messages in {name}"),
		| (Some(inviter), _) => {
			let room = latest
				.and_then(|room| room.name.as_deref())
				.unwrap_or("a room");

			format!("[{brand}] {inviter} invited you to {room}")
		},
		| (None, count) => {
			let others = count.saturating_sub(1);
			let noun = plural(others, "room", "rooms");

			format!("[{brand}] New messages in {name} and {others} other {noun}")
		},
	}
}

/// What the mail calls a room: its name, else its first sender shown.
///
/// A room with neither reads as an unnamed room.
#[implement(Room)]
fn label(&self) -> &str {
	self.name
		.as_deref()
		.or_else(|| self.first_sender())
		.unwrap_or(UNNAMED_ROOM)
}

#[implement(Room)]
fn first_sender(&self) -> Option<&str> {
	self.lines
		.first()
		.map(|line| line.sender.as_str())
}

/// The email's body.
///
/// Each room lists its notifications under its name, then its link.
pub(super) fn text(digest: &Digest<'_>) -> String {
	let header = format!("You have unread notifications on {}.\n\n", digest.brand);
	let body = digest.rooms.iter().fold(header, text_room);

	footer(body, digest)
}

fn text_room(mut out: String, room: &Room) -> String {
	writeln!(out, "{}", room.label()).ok();
	for line in &room.lines {
		writeln!(out, "  {} ({}): {}", line.sender, line.time, line.snippet).ok();
	}

	writeln!(out, "  {}\n", room.id.matrix_to_uri()).ok();
	out
}

fn footer(mut out: String, digest: &Digest<'_>) -> String {
	let (hidden, address, brand) = (digest.hidden, digest.address, digest.brand);

	if hidden > 0 {
		let noun = plural(hidden, "notification", "notifications");

		writeln!(out, "And {hidden} more {noun}.\n").ok();
	}

	writeln!(
		out,
		"You received this email because email notifications are on for {address} on your \
		 {brand} account. To stop them, turn email notifications off in your client's \
		 notification settings."
	)
	.ok();

	out
}
