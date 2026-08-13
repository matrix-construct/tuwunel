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

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, txn, servers))]
pub(crate) async fn add_servers_invite_via(
	&self,
	txn: &mut Txn,
	room_id: &RoomId,
	servers: Vec<OwnedServerName>,
) {
	let mut servers: Vec<_> = self
		.servers_invite_via(room_id)
		.map(ToOwned::to_owned)
		.chain(iter(servers.into_iter()))
		.collect()
		.await;

	servers.sort_unstable();
	servers.dedup();

	let servers = servers
		.iter()
		.map(|server| server.as_bytes())
		.collect_vec()
		.join(&[0xFF][..]);

	txn.insert_raw(&self.db.roomid_inviteviaservers, room_id.as_bytes(), &servers);
}

/// Gets up to five servers that are likely to be in the room in the
/// distant future.
///
/// See <https://spec.matrix.org/latest/appendices/#routing>
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

/// The room's highest power-level user's server, provided that user holds at
/// least power level 50.
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

/// Servers participating in the room, ordered by descending resident user
/// count. Counting members per server is an aggregation, so the result is
/// materialized rather than streamed.
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
