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
use tuwunel_database::Ignore;

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, servers))]
pub async fn add_servers_invite_via(&self, room_id: &RoomId, servers: Vec<OwnedServerName>) {
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

	self.db
		.roomid_inviteviaservers
		.insert(room_id.as_bytes(), &servers);
}

/// Gets up to five servers that are likely to be in the room in the
/// distant future.
///
/// See <https://spec.matrix.org/latest/appendices/#routing>
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "trace")]
pub async fn servers_route_via(&self, room_id: &RoomId) -> Result<Vec<OwnedServerName>> {
	let most_powerful_user_server = self
		.services
		.state_accessor
		.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
		.await
		.map(|content: RoomPowerLevelsEventContent| {
			content
				.users
				.iter()
				.max_by_key(|(_, power)| *power)
				.and_then(|x| (x.1 >= &int!(50)).then_some(x))
				.map(|(user, _power)| user.server_name().to_owned())
		});

	let mut servers: Vec<OwnedServerName> = self
		.room_members(room_id)
		.counts_by(|user| user.server_name().to_owned())
		.await
		.into_iter()
		.sorted_by_key(|(_, users)| *users)
		.map(|(server, _)| server)
		.rev()
		.take(5)
		.collect();

	if let Ok(Some(server)) = most_powerful_user_server {
		servers.insert(0, server);
		servers.truncate(5);
	}

	Ok(servers)
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
