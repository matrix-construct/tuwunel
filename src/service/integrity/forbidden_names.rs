//! Existing names the configuration forbids.
//!
//! The forbidden patterns refuse new usernames and aliases only, so a name taken
//! before a pattern was added stays in use. One check covers users, the other
//! aliases.

use std::{fmt, str::from_utf8};

use futures::{Stream, TryStreamExt};
use regex::RegexSet;
use ruma::{OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, UserId};
use tuwunel_core::{
	Result,
	itertools::Itertools,
	result::NotFound,
	utils::{BoolExt, TryReadyExt},
};
use tuwunel_database::SEP;

use super::{Check, Report, ScanExt, tally};
use crate::Services;

pub(super) static USERNAMES: Check = Check {
	name: "forbidden-usernames",
	about: "No active local user's localpart matches a forbidden username pattern",
	quiescent: false,
	walk: |services, limit| tally(usernames(services), limit),
};

pub(super) static ALIASES: Check = Check {
	name: "forbidden-aliases",
	about: "No local alias of a room this server knows matches a forbidden alias name pattern",
	quiescent: false,
	walk: |services, limit| tally(aliases(services), limit),
};

/// An active local user whose localpart a forbidden username pattern matches.
///
/// The account stays usable, so only an admin acting on it clears the finding.
pub struct Username {
	/// The user.
	pub user_id: OwnedUserId,

	/// The patterns the localpart matches, joined by commas.
	pub patterns: String,
}

/// A local alias whose name a forbidden alias name pattern matches.
///
/// The alias keeps resolving, so only an admin acting on it clears the finding.
pub struct Alias {
	/// The room the alias names.
	pub room_id: OwnedRoomId,

	/// The alias.
	pub alias: OwnedRoomAliasId,

	/// The patterns the alias name matches, joined by commas.
	pub patterns: String,
}

/// Streams each active local user a forbidden username pattern matches.
///
/// Every row of `userid_password` is read, and an active user is one whose row
/// holds a password or the passwordless sentinel. A key that is not a user id is
/// skipped, as the sentinel passwords check reports one that is not text.
#[tracing::instrument(level = "trace", skip_all)]
pub fn usernames(services: &Services) -> impl Stream<Item = Result<Username>> + Send + '_ {
	let patterns = &services.config.registration.forbidden_usernames;

	services.db["userid_password"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(move |(key, value)| Ok(username(services, patterns, key, value)))
}

/// Streams each local alias of a known room a forbidden alias name pattern
/// matches.
///
/// Every row of `aliasid_alias` is read, and a matching alias is reported only
/// when its room has a short id, as a room this server knows does. A row whose
/// key or value does not parse is skipped.
#[tracing::instrument(level = "trace", skip_all)]
pub fn aliases(services: &Services) -> impl Stream<Item = Result<Alias>> + Send + '_ {
	let patterns = &services.config.rooms.forbidden_alias_names;
	let rooms = &services.db["roomid_shortroomid"];

	// Serial, so a stop request ends the walk at the row after a finding.
	services.db["aliasid_alias"]
		.raw_scan()
		.scanned(&services.server)
		.ready_try_filter_map(move |(key, value)| Ok(alias(patterns, key, value)))
		.try_filter_map(async |alias| {
			let known = rooms
				.exists(alias.room_id.as_bytes())
				.await
				.optional()?;

			Ok(known.map(|()| alias))
		})
}

impl Report for Username {
	fn kind(&self) -> &'static str { "forbidden-username" }
}

impl fmt::Display for Username {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "user {} matches forbidden username patterns {}", self.user_id, self.patterns)
	}
}

impl Report for Alias {
	fn kind(&self) -> &'static str { "forbidden-alias" }
}

impl fmt::Display for Alias {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"alias {} of {} matches forbidden alias name patterns {}",
			self.alias, self.room_id, self.patterns
		)
	}
}

fn username(
	services: &Services,
	patterns: &RegexSet,
	key: &[u8],
	value: &[u8],
) -> Option<Username> {
	let user_id: &UserId = from_utf8(key).ok()?.try_into().ok()?;
	let active = services.globals.user_is_local(user_id) && !value.is_empty();
	let patterns = active.and_then(|| matched(patterns, user_id.localpart()))?;

	Some(Username { user_id: user_id.to_owned(), patterns })
}

fn alias(patterns: &RegexSet, key: &[u8], value: &[u8]) -> Option<Alias> {
	let at = key.iter().position(|&byte| byte == SEP)?;
	let (room_id, _) = key.split_at(at);
	let room_id: &RoomId = from_utf8(room_id).ok()?.try_into().ok()?;
	let alias: &RoomAliasId = from_utf8(value).ok()?.try_into().ok()?;
	let patterns = matched(patterns, alias.alias())?;
	let alias = Alias {
		room_id: room_id.to_owned(),
		alias: alias.to_owned(),
		patterns,
	};

	Some(alias)
}

pub(crate) fn matched(patterns: &RegexSet, name: &str) -> Option<String> {
	let matches = patterns.matches(name);

	matches.matched_any().then(|| {
		matches
			.iter()
			.map(|index| &patterns.patterns()[index])
			.join(", ")
	})
}
