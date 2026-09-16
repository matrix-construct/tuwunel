use std::{pin::pin, time::Duration};

use futures::{
	FutureExt, StreamExt, TryStreamExt,
	future::{join, pending},
	stream::{iter, once},
};
use ruma::{OwnedRoomId, RoomId, UserId, room_id, user_id};
use serde_json::json;
use tokio::{sync::oneshot::channel, time::timeout};
use tuwunel_core::{Err, Result, config::Figment, utils::result::NotFound};
use tuwunel_database::{Deserialized, Interfix, Json, Txn};

use super::super::Service;
use crate::{Services, test_utils::fixture};

const WAIT: Duration = Duration::from_secs(5);
const PAUSE: Duration = Duration::from_millis(20);
const FIELDS: [&str; 3] = ["com.example.first", "com.example.second", "com.example.third"];

#[tokio::test]
async fn publication_holds_count_and_notifies_after_all_rows_commit() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	publish_and_observe(&fixture.services, false).await?;
	publish_and_observe(&fixture.services, true).await
}

#[tokio::test]
async fn failed_preparation_discards_rows_and_retires_count() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.profile;
	let globals = &fixture.services.globals;
	let before = globals.current_count();
	let stalled = iter([Ok(rooms()[0])]).chain(once(async {
		pending::<()>().await;
		Ok(rooms()[1])
	}));

	let txn = transaction(service, false);

	timeout(PAUSE, service.publish_update(user(), &FIELDS, &FIELDS, txn, || stalled))
		.await
		.expect_err("stalled publication is dropped here");

	let failing = iter([Ok(rooms()[0]), Err!(Database("room scan failed"))]);
	let txn = transaction(service, false);

	service
		.publish_update(user(), &FIELDS, &FIELDS, txn, || failing)
		.await
		.expect_err("room scan failure aborts publication");

	let retired = timeout(WAIT, globals.wait_pending())
		.await
		.expect("abandoned counts retire")?;

	assert_eq!(retired, before.saturating_add(2));
	assert_absent(service).await?;

	assert_eq!(rows(service).await, 0);

	let txn = transaction(service, false);

	service
		.publish_update(user(), &FIELDS, &FIELDS, txn, || iter(rooms().map(Ok)))
		.await?;

	assert_present(service).await?;
	assert_discovery(service, before.saturating_add(3)).await
}

#[tokio::test]
async fn room_scan_error_aborts_clear_until_the_key_is_removed() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let service = &services.profile;
	let joined = &services.db["userroomid_joined"];
	let remote = user_id!("@remote:example.test");
	let neighbor = user_id!("@remote:example.test.extra");

	joined.put((remote, rooms()[0]), 1_u64);
	joined.put((remote, "junk"), 1_u64);
	joined.put((neighbor, rooms()[1]), 1_u64);
	service
		.useridprofilekey_value
		.put((remote, FIELDS[0]), Json(json!("stored")));

	let scanned: Vec<Result<OwnedRoomId>> = services
		.state_cache
		.rooms_joined_checked(remote)
		.map_ok(ToOwned::to_owned)
		.collect()
		.await;

	assert_eq!(scanned.len(), 2);
	assert_eq!(scanned[0].as_deref().ok(), Some(rooms()[0]));
	scanned[1]
		.as_ref()
		.expect_err("malformed room key is an error item");

	service
		.clear_profile_keys(remote)
		.await
		.expect_err("room scan error aborts the clear");

	let stored: String = service
		.profile_key(remote, &FIELDS[0].into())
		.await?;

	assert_eq!(stored, "stored");
	assert_eq!(rows(service).await, 0);
	joined.del((remote, "junk"));
	service.clear_profile_keys(remote).await?;

	let cleared: Option<String> = service
		.profile_key(remote, &FIELDS[0].into())
		.await
		.optional()?;

	assert_eq!(cleared, None);

	let count = timeout(WAIT, services.globals.wait_pending())
		.await
		.expect("clear count retires")?;

	for scope in [remote.as_str(), rooms()[0].as_str()] {
		let owner: String = service
			.profilechangeid_userid
			.qry(&(scope, count, FIELDS[0]))
			.await?
			.deserialized()?;

		assert_eq!(owner, remote.as_str());
	}

	assert_eq!(rows(service).await, 2);
	service.clear_profile_keys(remote).await?;
	assert_eq!(services.globals.current_count(), count);
	assert_eq!(rows(service).await, 2);
	Ok(())
}

async fn publish_and_observe(services: &Services, deletion: bool) -> Result {
	let service = &services.profile;
	let globals = &services.globals;
	let expected = globals.current_count().saturating_add(1);
	let (release, resume) = channel();
	let joined = iter([Ok(rooms()[0])]).chain(once(async {
		resume.await.expect("resume publication");
		Ok(rooms()[1])
	}));

	let txn = transaction(service, deletion);
	let mut owner = pin!(
		service
			.profilechangeid_userid
			.watch_prefix((user(), Interfix))
	);

	let mut room = pin!(
		service
			.profilechangeid_userid
			.watch_prefix((rooms()[1], Interfix))
	);

	let publication = service.publish_update(user(), &FIELDS, &FIELDS, txn, || joined);
	let mut publication = pin!(publication); // Pin::as_mut resumes it after the timeout

	timeout(PAUSE, publication.as_mut())
		.await
		.expect_err("publication waits for the room scan");

	timeout(PAUSE, globals.wait_pending())
		.await
		.expect_err("count stays pending");

	assert_eq!(globals.current_count().saturating_add(1), expected);
	assert!(owner.as_mut().now_or_never().is_none());
	assert!(room.as_mut().now_or_never().is_none());
	if deletion {
		assert_present(service).await?;
	} else {
		assert_absent(service).await?;
	}

	release
		.send(())
		.expect("publication still waiting");

	let observe = async {
		timeout(WAIT, join(owner, room))
			.await
			.expect("publication watchers");

		if deletion {
			assert_absent(service).await?;
		} else {
			assert_present(service).await?;
		}

		assert_discovery(service, expected).await
	};

	let (published, observed) = timeout(WAIT, join(publication, observe))
		.await
		.expect("publication completes");

	published?;
	observed?;

	let retired = timeout(WAIT, globals.wait_pending())
		.await
		.expect("count retired")?;

	assert_eq!(retired, expected);
	Ok(())
}

fn transaction(service: &Service, deletion: bool) -> Txn {
	FIELDS
		.iter()
		.fold(service.services.db.txn(), |mut txn, field| {
			if deletion {
				txn.del(&service.useridprofilekey_value, (user(), field));
			} else {
				txn.put(&service.useridprofilekey_value, (user(), field), Json(json!(field)));
			}

			txn
		})
}

async fn assert_present(service: &Service) -> Result {
	for field in FIELDS {
		let stored: String = service.profile_key(user(), &field.into()).await?;

		assert_eq!(stored, field);
	}

	Ok(())
}

async fn assert_absent(service: &Service) -> Result {
	for field in FIELDS {
		let stored: Option<String> = service
			.profile_key(user(), &field.into())
			.await
			.optional()?;

		assert_eq!(stored, None);
	}

	Ok(())
}

async fn assert_discovery(service: &Service, count: u64) -> Result {
	for scope in [user().as_str(), rooms()[0].as_str(), rooms()[1].as_str()] {
		for field in FIELDS {
			let owner: String = service
				.profilechangeid_userid
				.qry(&(scope, count, field))
				.await?
				.deserialized()?;

			assert_eq!(owner, user().as_str());
		}
	}

	Ok(())
}

async fn rows(service: &Service) -> usize {
	service
		.profilechangeid_userid
		.raw_keys()
		.count()
		.await
}

fn user() -> &'static UserId { user_id!("@publication:localhost") }

fn rooms() -> [&'static RoomId; 2] {
	[room_id!("!first:localhost"), room_id!("!second:localhost")]
}
