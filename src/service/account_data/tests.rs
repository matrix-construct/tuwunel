use std::{pin::pin, time::Duration};

use futures::{StreamExt, future::join};
use ruma::{
	RoomId,
	events::{RoomAccountDataEventType, tag::TagName},
	room_id, user_id,
};
use serde_json::{Value as JsonValue, json};
use tokio::time::timeout;
use tuwunel_core::{Result, config::Figment};

use crate::test_utils::fixture;

const WATCH_WINDOW: Duration = Duration::from_millis(500);

const KIND: &str = "org.example.revision";

const KEPT: &str = "u.kept";

const FIRST: &str = "u.first";

const SECOND: &str = "u.second";

const TRUNCATED: &[u8] = br#"{"type":"m.tag","content":"#;

/// Two updates of one type waiting on their user's lock leave one revision,
/// the last write's, which the pointer names.
///
/// Each update reads the pointer under the lock, so it replaces the revision
/// the other stored. Neither holds a sequence number while it waits, so the
/// sync frontier is not held behind the lock.
#[tokio::test]
async fn concurrent_updates_keep_one_revision() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let account_data = &fixture.services.account_data;
	let user_id = user_id!("@writer:localhost");
	let (initial, one, two) = (revision(0), revision(1), revision(2));

	account_data
		.update(None, user_id, KIND.into(), &initial)
		.await?;

	let updating = account_data.updating.lock(user_id).await;
	let mut first = pin!(account_data.update(None, user_id, KIND.into(), &one));
	let mut second = pin!(account_data.update(None, user_id, KIND.into(), &two));
	let (first_waited, second_waited) =
		join(timeout(WATCH_WINDOW, first.as_mut()), timeout(WATCH_WINDOW, second.as_mut())).await;

	assert!(
		first_waited.is_err() && second_waited.is_err(),
		"an update finished while its user's lock was held"
	);

	let frontier = timeout(WATCH_WINDOW, fixture.services.globals.wait_pending()).await;

	assert!(
		matches!(frontier, Ok(Ok(_))),
		"an update waiting on the lock holds a sequence number"
	);

	drop(updating);
	first.await?;
	second.await?;

	let revisions = account_data
		.changes_since(None, user_id, 0, None)
		.count()
		.await;

	assert_eq!(revisions, 1, "a superseded revision is left in the log");

	let stored: JsonValue = account_data
		.get_global(user_id, KIND.into())
		.await?;

	assert_eq!(stored, two);

	Ok(())
}

/// An erase takes its user's lock before it scans, so an update ahead of it
/// leaves nothing the scans miss.
///
/// The update holds the place ahead of the erase in the lock's queue. An erase
/// that scanned first would delete the pointer the update rewrote and keep the
/// new revision, which nothing would name.
#[tokio::test]
async fn erase_waits_for_a_running_update() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let account_data = &fixture.services.account_data;
	let user_id = user_id!("@eraser:localhost");
	let (initial, next) = (revision(0), revision(1));

	account_data
		.update(None, user_id, KIND.into(), &initial)
		.await?;

	let updating = account_data.updating.lock(user_id).await;
	let mut update = pin!(account_data.update(None, user_id, KIND.into(), &next));
	let mut erase = pin!(account_data.erase_user(user_id, None));
	let (update_waited, erase_waited) =
		join(timeout(WATCH_WINDOW, update.as_mut()), timeout(WATCH_WINDOW, erase.as_mut())).await;

	assert!(
		update_waited.is_err() && erase_waited.is_err(),
		"an update or an erase finished while its user's lock was held"
	);

	drop(updating);
	update.await?;
	erase.await;

	let revisions = account_data
		.changes_since(None, user_id, 0, None)
		.count()
		.await;

	assert_eq!(revisions, 0, "the erase left a value behind");

	let pointer = account_data
		.db
		.roomusertype_roomuserdataid
		.qry(&(None::<&RoomId>, user_id, KIND))
		.await;

	assert!(pointer.is_err(), "the erase left a pointer behind");

	Ok(())
}

/// Two tag writes waiting on their user's lock both land.
///
/// Each reads the stored set only once it holds the lock, so the second merges
/// into the set the first stored rather than into the one both would have read
/// before waiting.
#[tokio::test]
async fn concurrent_tag_writes_keep_every_tag() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let account_data = &fixture.services.account_data;
	let user_id = user_id!("@tagger:localhost");
	let room_id = room_id!("!tagged:localhost");

	account_data
		.set_room_tag(user_id, room_id, KEPT.into(), None)
		.await?;

	let updating = account_data.updating.lock(user_id).await;
	let mut first = pin!(account_data.set_room_tag(user_id, room_id, FIRST.into(), None));
	let mut second = pin!(account_data.set_room_tag(user_id, room_id, SECOND.into(), None));
	let (first_waited, second_waited) =
		join(timeout(WATCH_WINDOW, first.as_mut()), timeout(WATCH_WINDOW, second.as_mut())).await;

	assert!(
		first_waited.is_err() && second_waited.is_err(),
		"a tag write finished while its user's lock was held"
	);

	drop(updating);
	first.await?;
	second.await?;

	let tags = account_data
		.get_room_tags(user_id, room_id)
		.await?;

	assert!(
		[KEPT, FIRST, SECOND]
			.map(TagName::from)
			.iter()
			.all(|tag| tags.contains_key(tag)),
		"a concurrent tag write was lost: {tags:?}"
	);

	Ok(())
}

/// A tag write after the tag set was deleted starts from an empty set.
///
/// The MSC3391 tombstone's empty content does not read as a tag event, so a
/// merge taking it for an unreadable set would refuse every later tag.
#[tokio::test]
async fn tag_write_after_deletion_starts_empty() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let account_data = &fixture.services.account_data;
	let user_id = user_id!("@untagger:localhost");
	let room_id = room_id!("!untagged:localhost");

	account_data
		.set_room_tag(user_id, room_id, KEPT.into(), None)
		.await?;

	account_data
		.delete(Some(room_id), user_id, RoomAccountDataEventType::Tag)
		.await?;

	account_data
		.set_room_tag(user_id, room_id, FIRST.into(), None)
		.await?;

	let tags = account_data
		.get_room_tags(user_id, room_id)
		.await?;

	assert!(tags.keys().eq([&TagName::from(FIRST)]), "the deleted set came back: {tags:?}");

	Ok(())
}

/// Tag writes change only the named tag and keep fields the server does not model.
///
/// The stored event is edited as JSON, so another tag's extra metadata and a
/// sibling of the tag set survive an addition, and the removal that follows
/// leaves the event as it was stored.
#[tokio::test]
async fn tag_writes_keep_fields_they_do_not_model() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let account_data = &fixture.services.account_data;
	let user_id = user_id!("@annotator:localhost");
	let room_id = room_id!("!annotated:localhost");
	let stored = json!({
		"type": "m.tag",
		"content": {
			"tags": { "u.kept": { "order": 0.5, "org.example.color": "blue" } },
			"org.example.note": "kept",
		},
	});

	account_data
		.update(Some(room_id), user_id, RoomAccountDataEventType::Tag, &stored)
		.await?;

	account_data
		.set_room_tag(user_id, room_id, FIRST.into(), None)
		.await?;

	account_data
		.remove_room_tag(user_id, room_id, &FIRST.into())
		.await?;

	let merged: JsonValue = account_data
		.get_room(room_id, user_id, RoomAccountDataEventType::Tag)
		.await?;

	assert_eq!(merged, stored);

	Ok(())
}

/// A tag write refuses a stored set it cannot read and leaves it in place.
///
/// Starting from an empty set instead would replace whatever the unreadable
/// value held with the one new tag.
#[tokio::test]
async fn tag_write_keeps_an_unreadable_set() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let account_data = &fixture.services.account_data;
	let user_id = user_id!("@misfiler:localhost");
	let room_id = room_id!("!misfiled:localhost");
	let unreadable = json!({ "type": "m.tag", "content": { "tags": "unreadable" } });

	account_data
		.update(Some(room_id), user_id, RoomAccountDataEventType::Tag, &unreadable)
		.await?;

	let written = account_data
		.set_room_tag(user_id, room_id, FIRST.into(), None)
		.await;

	assert!(written.is_err(), "a tag write replaced an unreadable set");

	let stored: JsonValue = account_data
		.get_room(room_id, user_id, RoomAccountDataEventType::Tag)
		.await?;

	assert_eq!(stored, unreadable);

	Ok(())
}

/// A tag write refuses a stored value that does not decode and leaves it.
///
/// The failed read reaches the merge as an error, which the write returns
/// rather than starting the tag set over.
#[tokio::test]
async fn tag_write_keeps_an_undecodable_value() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let account_data = &fixture.services.account_data;
	let user_id = user_id!("@garbler:localhost");
	let room_id = room_id!("!garbled:localhost");

	account_data
		.set_room_tag(user_id, room_id, KEPT.into(), None)
		.await?;

	let pointer: Vec<u8> = account_data
		.db
		.roomusertype_roomuserdataid
		.qry(&(Some(room_id), user_id, "m.tag"))
		.await?
		.into();

	account_data
		.db
		.roomuserdataid_accountdata
		.insert(&pointer, TRUNCATED);

	let written = account_data
		.set_room_tag(user_id, room_id, FIRST.into(), None)
		.await;

	assert!(written.is_err(), "a tag write replaced an undecodable value");

	let stored = account_data
		.get_raw(Some(room_id), user_id, "m.tag")
		.await?;

	assert_eq!(&*stored, TRUNCATED);

	Ok(())
}

fn revision(n: u8) -> JsonValue { json!({ "type": KIND, "content": { "n": n } }) }
