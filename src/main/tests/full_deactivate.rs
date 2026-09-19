#![cfg(test)]

use std::{net::TcpListener, time::Duration};

use futures::{FutureExt, StreamExt, future::join};
use serde_json::json;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Result,
	ruma::{
		MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UserId, events::StateEventType,
		thirdparty::Medium,
	},
	utils::stream::ReadyExt,
};
use tuwunel_service::Services;

use self::client::{Client, poll_until, register, wait_until_ready};

mod client;

struct Deactivation<'a> {
	erased: &'a UserId,
	retained: &'a UserId,
	joined_a: &'a RoomId,
	joined_b: &'a RoomId,
	privileged: &'a RoomId,
	invited: &'a RoomId,
	knocked: &'a RoomId,
	left: &'a RoomId,
	retained_room: &'a RoomId,
}
const MEMBERSHIP_DEADLINE: Duration = Duration::from_secs(10);
const DATA_KIND: &str = "org.tuwunel.test.full_deactivate";
const ERASED_EMAIL: &str = "erased@example.invalid";
const ERASED_TOKEN: &str = "full-deactivate-erased-access-token";
const PEER_TOKEN: &str = "full-deactivate-peer-access-token-1";
const RETAINED_EMAIL: &str = "retained@example.invalid";
const RETAINED_TOKEN: &str = "full-deactivate-retained-access-token";

/// Exercises the shared deactivation lifecycle across each membership index.
///
/// Erasure removes non-event data from current and retained rooms, while plain
/// deactivation preserves it. Both paths leave and forget current rooms, but a
/// room that was already left keeps its retained leave row.
#[test]
#[tracing::instrument(skip_all, level = "debug")]
fn full_deactivation_preserves_the_erasure_boundary() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();
	let args = Args::default_test(&["fresh", "cleanup"])
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let driven = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (served, outcome) = join(async_run(&server), driven).await;

		drop(services);
		async_stop(&server).await?;
		served?;

		outcome
	});

	drop(runtime);

	result
}

#[tracing::instrument(skip_all, level = "debug")]
async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	let erased = register(services, "full-deactivate-erased", ERASED_TOKEN).await?;
	let retained = register(services, "full-deactivate-retained", RETAINED_TOKEN).await?;

	register(services, "full-deactivate-peer", PEER_TOKEN).await?;
	let erased_client = Client { services, base, token: ERASED_TOKEN };
	let retained_client = Client { services, base, token: RETAINED_TOKEN };
	let peer_client = Client { services, base, token: PEER_TOKEN };

	// MSC4289 rooms never list the creator in power_levels.users, so the
	// entries whose erasure is asserted below need a version 11 room.
	let listed_creator_room = json!({ "preset": "private_chat", "room_version": "11" });

	let joined_a = erased_client
		.create_room(&listed_creator_room)
		.await?;

	let joined_b = erased_client
		.create_room(&listed_creator_room)
		.await?;

	// Version 12 creators are privileged without a power-levels entry.
	let privileged = erased_client
		.create_room(&json!({ "preset": "private_chat", "room_version": "12" }))
		.await?;

	let left = erased_client
		.create_room(&json!({ "preset": "private_chat" }))
		.await?;

	erased_client
		.post(&format!("rooms/{left}/leave"), &json!({}))
		.await?;

	let invited = peer_client
		.create_room(&json!({ "preset": "private_chat", "invite": [&erased] }))
		.await?;

	let knock_room = json!({
		"preset": "public_chat",
		"initial_state": [{
			"type": "m.room.join_rules",
			"state_key": "",
			"content": { "join_rule": "knock" },
		}],
	});

	let knocked = peer_client.create_room(&knock_room).await?;

	erased_client
		.post(&format!("knock/{knocked}"), &json!({}))
		.await?;

	let retained_room = retained_client
		.create_room(&listed_creator_room)
		.await?;

	assert!(
		poll_until(MEMBERSHIP_DEADLINE, async || {
			services
				.state_cache
				.is_invited(&erased, &invited)
				.await
		})
		.await,
		"invite row did not appear",
	);

	assert!(
		services
			.state_cache
			.is_knocked(&erased, &knocked)
			.await
	);

	assert!(services.state_cache.is_left(&erased, &left).await);

	services
		.profile
		.set_displayname(&erased, Some("Erased User"), None)
		.await?;

	services
		.profile
		.set_displayname(&retained, Some("Retained User"), None)
		.await?;

	services
		.threepid
		.put_binding(
			&erased,
			ERASED_EMAIL,
			Medium::Email,
			MilliSecondsSinceUnixEpoch::now(),
			MilliSecondsSinceUnixEpoch::now(),
		)
		.await;

	services
		.threepid
		.put_binding(
			&retained,
			RETAINED_EMAIL,
			Medium::Email,
			MilliSecondsSinceUnixEpoch::now(),
			MilliSecondsSinceUnixEpoch::now(),
		)
		.await;

	put_data(services, &erased, None).await?;
	for room_id in [&joined_a, &joined_b, &invited, &knocked, &left] {
		put_data(services, &erased, Some(room_id)).await?;
	}

	put_data(services, &retained, None).await?;
	put_data(services, &retained, Some(&retained_room)).await?;
	assert!(has_data(services, &erased, None).await);

	for room_id in [&joined_a, &joined_b, &invited, &knocked, &left] {
		assert!(
			has_data(services, &erased, Some(room_id)).await,
			"account data missing for {room_id}",
		);
	}

	assert!(has_data(services, &retained, None).await);
	assert!(has_data(services, &retained, Some(&retained_room)).await);

	assert_power_level_users(
		services,
		[(&erased, &joined_a), (&erased, &joined_b), (&retained, &retained_room)],
		true,
	)
	.await?;

	let deactivation = Deactivation {
		erased: &erased,
		retained: &retained,
		joined_a: &joined_a,
		joined_b: &joined_b,
		privileged: &privileged,
		invited: &invited,
		knocked: &knocked,
		left: &left,
		retained_room: &retained_room,
	};

	deactivate_and_assert(services, deactivation)
		.boxed() // size firewall
		.await
}

#[tracing::instrument(skip_all, level = "debug")]
async fn deactivate_and_assert(
	services: &Services,
	Deactivation {
		erased,
		retained,
		joined_a,
		joined_b,
		privileged,
		invited,
		knocked,
		left,
		retained_room,
	}: Deactivation<'_>,
) -> Result {
	let power_levels_before = power_levels_event_id(services, privileged).await?;

	services
		.deactivate
		.full_deactivate(erased, true)
		.await?;

	services
		.deactivate
		.full_deactivate(retained, false)
		.await?;

	assert!(services.users.is_deactivated(erased).await?);
	assert!(services.users.is_deactivated(retained).await?);
	assert!(services.users.is_erased(erased).await);
	assert!(!services.users.is_erased(retained).await);
	assert!(
		services
			.profile
			.displayname(erased)
			.await
			.is_err()
	);

	assert!(
		services
			.profile
			.displayname(retained)
			.await
			.is_err()
	);

	let erased_binding_count = services
		.threepid
		.get_bindings(erased)
		.count()
		.await;

	assert_eq!(erased_binding_count, 0);

	let (retained_binding_count, matching_binding_count) = services
		.threepid
		.get_bindings(retained)
		.ready_fold((0_usize, 0_usize), |(total, matches), binding| {
			(
				total.saturating_add(1),
				matches.saturating_add(usize::from(binding.address == RETAINED_EMAIL)),
			)
		})
		.await;

	assert_eq!(retained_binding_count, 1);
	assert_eq!(matching_binding_count, 1);

	for room_id in [joined_a, joined_b, privileged] {
		assert!(
			!services
				.state_cache
				.is_joined(erased, room_id)
				.await,
			"joined row retained for {room_id}",
		);

		assert!(
			!services
				.state_cache
				.is_left(erased, room_id)
				.await,
			"left row created for {room_id}",
		);
	}

	assert!(
		!services
			.state_cache
			.is_invited(erased, invited)
			.await
	);

	assert!(
		!services
			.state_cache
			.is_left(erased, invited)
			.await
	);

	assert!(
		!services
			.state_cache
			.is_knocked(erased, knocked)
			.await
	);

	assert!(
		!services
			.state_cache
			.is_left(erased, knocked)
			.await
	);

	assert!(services.state_cache.is_left(erased, left).await);

	assert!(
		!services
			.state_cache
			.is_joined(retained, retained_room)
			.await
	);

	assert!(
		!services
			.state_cache
			.is_left(retained, retained_room)
			.await
	);

	assert_power_level_users(
		services,
		[(erased, joined_a), (erased, joined_b), (retained, retained_room)],
		false,
	)
	.await?;

	assert_eq!(
		power_levels_event_id(services, privileged).await?,
		power_levels_before,
		"privileged creator demotion emitted a power-levels event",
	);

	assert!(!has_data(services, erased, None).await);

	for room_id in [joined_a, joined_b, invited, knocked, left] {
		assert!(
			!has_data(services, erased, Some(room_id)).await,
			"account data retained for {room_id}",
		);
	}

	assert!(has_data(services, retained, None).await);
	assert!(has_data(services, retained, Some(retained_room)).await);

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn power_levels_event_id(services: &Services, room_id: &RoomId) -> Result<OwnedEventId> {
	services
		.state_accessor
		.room_state_get_id(room_id, &StateEventType::RoomPowerLevels, "")
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn put_data(services: &Services, user_id: &UserId, room_id: Option<&RoomId>) -> Result {
	let event = json!({ "type": DATA_KIND, "content": { "value": true } });

	services
		.account_data
		.update(room_id, user_id, DATA_KIND.to_owned().into(), &event)
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn has_data(services: &Services, user_id: &UserId, room_id: Option<&RoomId>) -> bool {
	services
		.account_data
		.get_raw(room_id, user_id, DATA_KIND)
		.await
		.is_ok()
}

#[tracing::instrument(skip_all, level = "debug")]
async fn assert_power_level_users(
	services: &Services,
	users: [(&UserId, &RoomId); 3],
	expected: bool,
) -> Result {
	for (user_id, room_id) in users {
		let power_levels = services
			.state_accessor
			.get_power_levels(room_id)
			.await?;

		assert_eq!(
			power_levels.users.contains_key(user_id),
			expected,
			"power-level membership mismatch for {user_id} in {room_id}",
		);
	}

	Ok(())
}
