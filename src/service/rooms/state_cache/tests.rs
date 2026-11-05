use futures::StreamExt;
use ruma::{
	OwnedRoomId, event_id, events::room::member::MembershipState, room_id, serde::Raw, user_id,
};
use serde_json::{Value as JsonValue, json};
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::Json;
use tuwunel_matrix::PduCount;

use super::MembershipUpdate;
use crate::test_utils::{fixture, json_events, membership_update};

/// A user's room lists hold only the rows keyed by that exact user ID, not
/// those of a user whose server name extends hers.
#[tokio::test]
async fn room_lists_stop_at_the_user_id_boundary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let alice = user_id!("@alice:example.test");
	let neighbor = user_id!("@alice:example.test.extra");
	let own = room_id!("!own:example.test");
	let other = room_id!("!other:example.test.extra");

	for map in [
		"userroomid_joined",
		"userroomid_invitestate",
		"userroomid_knockedstate",
		"userroomid_leftstate",
	] {
		services.db[map].put_raw((alice, own), b"");
		services.db[map].put_raw((neighbor, other), b"");
	}

	let state_cache = &services.state_cache;
	// heterogeneous stream array
	let lists = [
		state_cache.rooms_joined(alice).boxed(),
		state_cache.rooms_invited(alice).boxed(),
		state_cache.rooms_knocked(alice).boxed(),
		state_cache.rooms_left(alice).boxed(),
	];

	for rooms in lists {
		let rooms: Vec<OwnedRoomId> = rooms.map(ToOwned::to_owned).collect().await;

		assert_eq!(rooms, [own.to_owned()]);
	}

	Ok(())
}

/// A replayed knock carrying no stripped state keeps the summary stored with
/// the knock.
///
/// A remote join by another local user replays the knock with no stripped
/// state. A replay carrying an empty list keeps the stored summary too.
#[tokio::test]
async fn a_replayed_knock_keeps_its_stored_summary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let state_cache = &fixture.services.state_cache;
	let alice = user_id!("@alice:localhost");
	let room_id = room_id!("!knock:example.test");
	let name_event = json!({
		"type": "m.room.name", "state_key": "", "sender": "@bob:example.test",
		"content": {"name": "Knock"},
	});

	let summary = vec![Raw::new(&name_event)?.cast_unchecked()];
	let knock = |count, last_state| MembershipUpdate {
		last_state,
		update_joined_count: false,
		..membership_update(room_id, alice, MembershipState::Knock, count)
	};

	state_cache
		.update_membership(knock(1, Some(summary)))
		.await?;

	for replay in [knock(2, None), knock(3, Some(Vec::new()))] {
		state_cache.update_membership(replay).await?;
	}

	let stored = state_cache.knock_state(alice, room_id).await?;

	assert_eq!(stored.len(), 1, "the stored summary survives the replays");

	let stored_event: JsonValue = stored[0].deserialize_as_unchecked()?;

	assert_eq!(stored_event, name_event);

	Ok(())
}

#[tokio::test]
async fn invite_event_follows_the_count_row() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let cache = &services.state_cache;
	let room = room_id!("!invite-tail:example.test");
	let user = user_id!("@alice:localhost");
	let first = event_id!("$first");
	let second = event_id!("$second");

	for (count, invite_event) in [(1, Some(first)), (2, Some(second)), (3, None)] {
		cache
			.update_membership(MembershipUpdate {
				invite_event,
				last_state: Some(Vec::new()),
				update_joined_count: false,
				..membership_update(room, user, MembershipState::Invite, count)
			})
			.await?;

		assert_eq!(cache.get_invite_count(room, user).await?, count);
		let stored = cache.get_invite_event(room, user).await?;

		assert_eq!(
			stored.as_deref(),
			invite_event,
			"re-invite did not replace or clear its event ID"
		);
	}

	for tail in [
		b"\x00\x00\x00\x00\x00\x00\x00\x03\xff\xffjunk".as_slice(),
		b"\x00\x00\x00\x00\x00\x00\x00\x03$i",
		b"\x00\x00\x00\x00\x00\x00\x00\x03\xff$i\xffjunk",
	] {
		services.db["roomuserid_invitecount"].put_raw((room, user), tail);
		assert!(cache.get_invite_event(room, user).await.is_err(), "bad framing accepted");
	}

	cache
		.update_membership(MembershipUpdate {
			invite_event: Some(first),
			update_joined_count: false,
			..membership_update(room, user, MembershipState::Invite, 4)
		})
		.await?;

	cache
		.update_membership(membership_update(room, user, MembershipState::Leave, 5))
		.await?;

	assert!(cache.get_invite_event(room, user).await.is_err(), "leave kept the invite ID");

	Ok(())
}

/// A knock summary is read with only the last of the user's own member events.
///
/// An earlier version stored a re-knock's summary with the user's previous
/// leave ahead of the knock, and a client reading the first member event about
/// itself would take the room for an invite.
#[tokio::test]
async fn a_knock_summary_reads_only_the_latest_own_membership() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let alice = user_id!("@alice:localhost");
	let room_id = room_id!("!knock:example.test");
	let member = |membership| {
		json!({
			"type": "m.room.member", "state_key": alice, "sender": alice,
			"content": {"membership": membership},
		})
	};

	let name_event = json!({
		"type": "m.room.name", "state_key": "", "sender": "@bob:example.test",
		"content": {"name": "Knock"},
	});

	let stored = json!([member("leave"), name_event, member("knock")]);

	services.db["userroomid_knockedstate"].put((alice, room_id), Json(&stored));

	let state_cache = &services.state_cache;
	let read = state_cache.knock_state(alice, room_id).await?;
	let listed: Vec<_> = state_cache
		.rooms_knocked_state(alice)
		.map(|(_, state)| state)
		.collect()
		.await;

	assert_eq!(json_events(&read)?, [name_event.clone(), member("knock")]);
	assert_eq!(listed.len(), 1);
	assert_eq!(json_events(&listed[0])?, [name_event, member("knock")]);

	Ok(())
}

/// A profile change keeps a recorded join position no later than the join it
/// replaces.
///
/// Any other join records its own position even while the user is joined, as a
/// replayed join does. A profile change after it then records one too. So does
/// a profile change over a legacy empty row or over no join row.
#[tokio::test]
async fn a_profile_change_keeps_the_join_position() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let state_cache = &services.state_cache;
	let alice = user_id!("@alice:localhost");
	let room_id = room_id!("!joined:example.test");
	let update = |membership, count| membership_update(room_id, alice, membership, count);
	let profile = |replaced, count| MembershipUpdate {
		replaced_join: Some(PduCount::Normal(replaced)),
		..update(MembershipState::Join, count)
	};

	let joined_after = async |update| {
		state_cache.update_membership(update).await?;
		state_cache.get_joined_count(room_id, alice).await
	};

	let join = |count| update(MembershipState::Join, count);

	assert_eq!(joined_after(join(1)).await?, 1, "a join recorded no position");
	assert_eq!(joined_after(profile(1, 2)).await?, 1, "a profile change moved the position");
	assert_eq!(joined_after(join(3)).await?, 3, "a join while joined kept the old position");
	assert_eq!(
		joined_after(profile(1, 4)).await?,
		4,
		"a profile change kept a later join's position"
	);

	services.db["roomuserid_joined"].put_raw((room_id, alice), b"");
	services.db["userroomid_joined"].put_raw((alice, room_id), b"");

	assert_eq!(
		joined_after(profile(4, 5)).await?,
		5,
		"a profile change kept a legacy row's empty position"
	);

	state_cache
		.update_membership(update(MembershipState::Leave, 6))
		.await?;

	assert_eq!(
		joined_after(profile(5, 7)).await.ok(),
		Some(7),
		"a profile change over no join row recorded none"
	);

	Ok(())
}
