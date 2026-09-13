use futures::StreamExt;
use ruma::{
	OwnedRoomAliasId, RoomAliasId, RoomId, UserId, api::error::ErrorKind, room_alias_id, room_id,
	user_id,
};
use tuwunel_core::{Result, config::Figment, smallvec::SmallVec, utils::result::NotFound};

use super::Service;
use crate::test_utils::fixture;

type Aliases = SmallVec<[OwnedRoomAliasId; 2]>;

#[tokio::test]
async fn selective_deletion_preserves_other_aliases() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.alias;
	let room = room_id!("!room:localhost");
	let other = room_id!("!other:localhost");
	let removed = room_alias_id!("#removed:localhost");
	let survivor = room_alias_id!("#survivor:localhost");
	let unrelated = room_alias_id!("#unrelated:localhost");
	let alice = user_id!("@alice:localhost");
	let bob = user_id!("@bob:localhost");

	service.set_alias_by(removed, room, alice)?;
	service.set_alias_by(survivor, room, bob)?;
	service.set_alias_by(unrelated, other, alice)?;
	listing(service, room, &[removed, survivor]).await;
	listing(service, other, &[unrelated]).await;
	present(service, removed, room, alice).await?;
	present(service, survivor, room, bob).await?;
	present(service, unrelated, other, alice).await?;
	assert_eq!(service.remove_alias(removed).await?, room);
	listing(service, room, &[survivor]).await;
	listing(service, other, &[unrelated]).await;
	absent(service, removed).await?;
	present(service, survivor, room, bob).await?;
	present(service, unrelated, other, alice).await?;

	Ok(())
}

#[tokio::test]
async fn last_alias_and_missing_deletions_preserve_other_rooms() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.alias;
	let room = room_id!("!room:localhost");
	let other = room_id!("!other:localhost");
	let last = room_alias_id!("#last:localhost");
	let unrelated = room_alias_id!("#unrelated:localhost");
	let never = room_alias_id!("#never:localhost");
	let creator = user_id!("@creator:localhost");

	service.set_alias_by(last, room, creator)?;
	service.set_alias_by(unrelated, other, creator)?;
	listing(service, room, &[last]).await;
	listing(service, other, &[unrelated]).await;
	present(service, last, room, creator).await?;
	present(service, unrelated, other, creator).await?;
	assert_eq!(service.remove_alias(last).await?, room);
	listing(service, room, &[]).await;
	absent(service, last).await?;

	for missing in [last, never] {
		let error = service
			.remove_alias(missing)
			.await
			.expect_err("missing alias must reject deletion");

		assert_eq!(error.kind(), ErrorKind::NotFound);
		listing(service, room, &[]).await;
		listing(service, other, &[unrelated]).await;
		absent(service, missing).await?;
		present(service, unrelated, other, creator).await?;
	}

	Ok(())
}

#[tokio::test]
async fn deleted_alias_can_be_recreated_in_another_room() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.alias;
	let room = room_id!("!room:localhost");
	let other = room_id!("!other:localhost");
	let moved = room_alias_id!("#moved:localhost");
	let unrelated = room_alias_id!("#unrelated:localhost");
	let alice = user_id!("@alice:localhost");
	let bob = user_id!("@bob:localhost");

	service.set_alias_by(moved, room, alice)?;
	service.set_alias_by(unrelated, other, alice)?;
	listing(service, room, &[moved]).await;
	listing(service, other, &[unrelated]).await;
	present(service, moved, room, alice).await?;
	present(service, unrelated, other, alice).await?;
	assert_eq!(service.remove_alias(moved).await?, room);
	listing(service, room, &[]).await;
	absent(service, moved).await?;
	service.set_alias_by(moved, other, bob)?;
	listing(service, room, &[]).await;
	listing(service, other, &[moved, unrelated]).await;
	present(service, moved, other, bob).await?;
	present(service, unrelated, other, alice).await?;
	assert_eq!(service.remove_alias(moved).await?, other);
	listing(service, room, &[]).await;
	listing(service, other, &[unrelated]).await;
	absent(service, moved).await?;
	present(service, unrelated, other, alice).await?;

	Ok(())
}

#[tokio::test]
async fn deletion_removes_every_matching_legacy_row() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.alias;
	let room = room_id!("!room:localhost");
	let duplicated = room_alias_id!("#duplicated:localhost");
	let survivor = room_alias_id!("#survivor:localhost");
	let creator = user_id!("@creator:localhost");

	service.set_alias_by(duplicated, room, creator)?;
	// Reassignment reproduces legacy duplicate reverse rows.
	service.set_alias_by(duplicated, room, creator)?;
	service.set_alias_by(survivor, room, creator)?;
	listing(service, room, &[duplicated, duplicated, survivor]).await;
	present(service, duplicated, room, creator).await?;
	present(service, survivor, room, creator).await?;
	assert_eq!(service.remove_alias(duplicated).await?, room);
	listing(service, room, &[survivor]).await;
	absent(service, duplicated).await?;
	present(service, survivor, room, creator).await?;

	Ok(())
}

async fn listing(service: &Service, room: &RoomId, expected: &[&RoomAliasId]) {
	let aliases = service
		.local_aliases_for_room(room)
		.map(ToOwned::to_owned)
		.collect()
		.await;

	assert_eq!(sorted(aliases).as_slice(), expected);
}

fn sorted(mut aliases: Aliases) -> Aliases {
	aliases.sort_unstable();
	aliases
}

async fn present(
	service: &Service,
	alias: &RoomAliasId,
	room: &RoomId,
	creator: &UserId,
) -> Result {
	assert_eq!(service.resolve_local_alias(alias).await?, room);
	assert_eq!(service.who_created_alias(alias).await?, creator);

	Ok(())
}

async fn absent(service: &Service, alias: &RoomAliasId) -> Result {
	assert_eq!(
		service
			.resolve_local_alias(alias)
			.await
			.optional()?,
		None
	);

	assert_eq!(
		service
			.who_created_alias(alias)
			.await
			.optional()?,
		None
	);

	Ok(())
}
