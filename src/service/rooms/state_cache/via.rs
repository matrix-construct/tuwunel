//! Routing-server hints derived from membership and invitation state.
//!
//! Power levels and joined-member distribution produce prospective routing
//! servers, while inbound invite hints are kept in a compact aggregate row.
//! The aggregate reader exposes only the final stored server from each row.

use std::cmp::Reverse;

use futures::{Stream, StreamExt, stream::iter};
use ruma::{
	OwnedServerName, RoomId, ServerName,
	events::{StateEventType, room::power_levels::RoomPowerLevelsEventContent},
	int,
};
use tuwunel_core::{
	Result, implement,
	itertools::Itertools,
	utils::{StreamTools, stream::TryIgnore},
	warn,
};
use tuwunel_database::{Ignore, Txn};

/// Merges invitation routing hints into the caller's membership transaction.
///
/// The existing reader exposes only the aggregate row's final server, so older
/// hints before that tail are not retained by this rewrite. Concurrent callers
/// are not serialized and can overwrite one another's read-modify-write result.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, txn, servers))]
pub(crate) async fn add_servers_invite_via(
	&self,
	txn: &mut Txn,
	room_id: &RoomId,
	servers: Vec<OwnedServerName>,
) {
	let servers = self
		.servers_invite_via(room_id)
		.map(ToOwned::to_owned)
		.chain(iter(servers.into_iter()))
		.collect::<Vec<_>>()
		.await
		.into_iter()
		.sorted_unstable()
		.dedup()
		.collect_vec();

	let servers = servers
		.iter()
		.map(|server| server.as_bytes())
		.collect_vec()
		.join(&[0xFF][..]);

	txn.insert_raw(&self.db.roomid_inviteviaservers, room_id.as_bytes(), &servers);
}

/// Selects up to five servers likely to remain useful for room routing.
///
/// The highest-power user's server is considered first, followed by servers in
/// descending joined-user count. The two sources are not deduplicated, and a
/// missing power-level event simply omits the first candidate.
///
/// See <https://spec.matrix.org/latest/appendices/#routing>.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn servers_route_via(&self, room_id: &RoomId) -> Result<Vec<OwnedServerName>> {
	let most_powerful = self.most_powerful_user_server(room_id).await;

	Ok(most_powerful
		.into_iter()
		.chain(self.popular_servers(room_id).await)
		.take(5)
		.collect())
}

/// Returns the highest-power user's server when its level is at least 50.
///
/// Missing, unreadable, or malformed power-level state returns `None`.
/// Equal-power ties follow the underlying users map's iteration order.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn most_powerful_user_server(&self, room_id: &RoomId) -> Option<OwnedServerName> {
	self.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
		.await
		.ok()
		.and_then(|content: RoomPowerLevelsEventContent| {
			content
				.users
				.into_iter()
				.max_by_key(|(_, power)| *power)
				.filter(|(_, power)| *power >= int!(50))
				.map(|(user, _)| user.server_name().to_owned())
		})
}

/// Returns participating servers ordered by descending joined-user count.
///
/// Joined members are counted per server and the resulting servers are sorted.
/// Read failures skipped by the membership stream can reduce the observed
/// counts, and equal-count ordering is unspecified.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn popular_servers(&self, room_id: &RoomId) -> Vec<OwnedServerName> {
	self.room_members(room_id)
		.counts_by(|user| user.server_name().to_owned())
		.await
		.into_iter()
		.sorted_by_key(|(_, users)| Reverse(*users))
		.map(|(server, _)| server)
		.collect()
}

/// Streams the final routing hint from each matching aggregate row.
///
/// The current room-keyed representation stores several servers in one value,
/// but this accessor exposes only the last decoded server and therefore at most
/// one item per room. Storage and decoding failures are skipped. Yielded names
/// borrow the cursor and are invalid after the next poll; consume or own each
/// item before advancing.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn servers_invite_via<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = &ServerName> + Send + 'a {
	type KeyVal<'a> = (Ignore, Vec<&'a ServerName>);

	self.db
		.roomid_inviteviaservers
		.stream_raw_prefix(room_id)
		.ignore_err()
		.map(|(_, servers): KeyVal<'_>| *servers.last().expect("at least one server"))
}
