#![cfg(test)]

use futures::StreamExt;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, PduCount, Result,
	ruma::{
		RoomId, ServerName, UserId,
		events::room::member::{MembershipState, RoomMemberEventContent},
		room_id, server_name, user_id,
	},
	utils::stream::ReadyExt,
};
use tuwunel_service::{Services, rooms::state_cache::MembershipUpdate};

#[test]
fn membership_updates_preserve_counts_servers_and_forgetting() -> Result {
	let args = Args::default_test(&["fresh", "cleanup"]);
	let option = args
		.option
		.into_iter()
		.chain(["forget_forced_upon_leave=false".into()])
		.collect();

	let args = Args { maintenance: true, option, ..args };
	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	})
}

async fn exercise(services: &Services) -> Result {
	let room = room_id!("!counts:example.org");
	let retained = user_id!("@one:retained.example");
	let duplicate = user_id!("@two:retained.example");
	let removed = user_id!("@one:removed.example");
	let added = user_id!("@one:added.example");
	let invited = user_id!("@invite:invited.example");
	let knocked = user_id!("@knock:knocked.example");

	for user in [retained, duplicate, removed] {
		transition(services, room, user, MembershipState::Join).await?;
	}

	transition(services, room, invited, MembershipState::Invite).await?;
	transition(services, room, knocked, MembershipState::Knock).await?;
	assert_aggregates(services, room, [3, 1, 1], [true, true, false]).await?;

	transition(services, room, duplicate, MembershipState::Leave).await?;
	transition(services, room, removed, MembershipState::Leave).await?;
	transition(services, room, added, MembershipState::Join).await?;
	assert_aggregates(services, room, [2, 1, 1], [true, false, true]).await?;

	for user in [retained, added, invited, knocked] {
		transition(services, room, user, MembershipState::Leave).await?;
	}

	assert_aggregates(services, room, [0, 0, 0], [false; 3]).await?;
	assert_forgetting(services).await
}

async fn transition(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
	membership: MembershipState,
) -> Result {
	services
		.state_cache
		.update_membership(MembershipUpdate {
			room_id,
			user_id,
			membership_event: RoomMemberEventContent::new(membership),
			sender: user_id,
			last_state: None,
			invite_via: None,
			update_joined_count: false,
			count: PduCount::Normal(*services.globals.next_count()),
		})
		.await
}

async fn assert_aggregates(
	services: &Services,
	room: &RoomId,
	expected: [u64; 3],
	present: [bool; 3],
) -> Result {
	let cache = &services.state_cache;

	cache.update_joined_count(room).await;

	let counts = [
		cache.room_joined_count(room).await?,
		cache.room_invited_count(room).await?,
		cache.room_knocked_count(room).await?,
	];

	if counts != expected {
		return Err!("membership counts {counts:?} != {expected:?}");
	}

	let servers: [&ServerName; 3] = [
		server_name!("retained.example"),
		server_name!("removed.example"),
		server_name!("added.example"),
	];

	for (server, expected) in servers.into_iter().zip(present) {
		let forward = cache
			.room_servers(room)
			.ready_any(|found| found == server)
			.await;

		let reverse = cache.server_in_room(server, room).await;

		if forward != expected || reverse != expected {
			return Err!("server indexes disagree with expected membership for {server}");
		}
	}

	if cache.room_servers(room).count().await != present.into_iter().filter(|p| *p).count() {
		return Err!("unexpected server in membership index");
	}

	Ok(())
}

async fn assert_forgetting(services: &Services) -> Result {
	let local = UserId::parse_with_server_name("leaving", services.globals.server_name())?;
	let remote = user_id!("@leaving:remote.example");
	let cases = [
		(room_id!("!ordinary:example.org"), false, false, true),
		(room_id!("!banned:example.org"), true, false, false),
		(room_id!("!disabled:example.org"), false, true, false),
		(room_id!("!both:example.org"), true, true, false),
	];

	for (room, banned, disabled, retained) in cases {
		if banned {
			services.metadata.ban_room(room);
		}

		if disabled {
			services.metadata.disable_room(room);
		}

		transition(services, room, &local, MembershipState::Leave).await?;
		transition(services, room, remote, MembershipState::Leave).await?;

		if services.state_cache.is_left(&local, room).await != retained {
			return Err!("incorrect local leave retention for {room}");
		}

		if !services.state_cache.is_left(remote, room).await {
			return Err!("remote leave was unexpectedly forgotten for {room}");
		}
	}

	Ok(())
}
