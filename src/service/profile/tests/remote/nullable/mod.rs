use std::iter::once;

use ruma::{
	OwnedUserId,
	api::{error::ErrorKind, federation::query::get_profile_information::v1::Response},
	room_id, user_id,
};
use serde_json::json;
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::{Deserialized, Json};

use super::{KEPT, field};
use crate::{profile::MAX_PROFILE_SIZE, test_utils::fixture};

#[tokio::test]
async fn repeated_nulls_and_replacements_publish_only_changes() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let user = user_id!("@nullable:remote.example");
	let room = room_id!("!nullable:localhost");
	let names = ["displayname", "avatar_url", "m.tz"];
	let response: Response = names
		.iter()
		.map(|name| ((*name).to_owned(), json!(null)))
		.collect();

	services.users.create(user, None, None).await?;
	services.db["userroomid_joined"].put((user, room), 1_u64);

	services
		.profile
		.merge_profile(user, response.clone())
		.await?;

	let before = services.globals.current_count();

	services
		.profile
		.merge_profile(user, response)
		.await?;

	services
		.profile
		.merge_profile(user, Response::default())
		.await?;

	assert_eq!(services.globals.current_count(), before);

	for name in names {
		assert_eq!(field(&services.profile, user, name).await?, json!(null));
	}

	let replacement: Response = [
		("displayname".to_owned(), json!("After")),
		("avatar_url".to_owned(), json!("mxc://remote.example/new")),
		("m.tz".to_owned(), json!("America/Denver")),
	]
	.into_iter()
	.collect();

	services
		.profile
		.merge_profile(user, replacement.clone())
		.await?;

	let count = services.globals.current_count();

	assert_eq!(count, before + 1);

	for (name, value) in replacement.iter() {
		assert_eq!(field(&services.profile, user, name).await?, *value);

		for scope in [user.as_str(), room.as_str()] {
			let row = services
				.profile
				.profilechangeid_userid
				.qry(&(scope, count, name))
				.await?;

			let owner: OwnedUserId = row.deserialized()?;

			assert_eq!(owner.as_str(), user.as_str());
		}
	}

	services
		.profile
		.merge_profile(user, replacement)
		.await?;

	assert_eq!(services.globals.current_count(), count);

	Ok(())
}

#[tokio::test]
async fn old_null_rows_remain_in_the_bounded_union() -> Result {
	let config = Figment::new().merge(("max_remote_profile_fields", 2));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let user = user_id!("@old-null:remote.example");
	let service = &services.profile;

	services.users.create(user, None, None).await?;
	service
		.useridprofilekey_value
		.put((user, "avatar_url"), Json(json!(null)));

	service
		.useridprofilekey_value
		.put((user, "displayname"), Json(json!(null)));

	let before = services.globals.current_count();
	let extra: Response = once((KEPT.to_owned(), json!("excess"))).collect();

	let error = service
		.merge_profile(user, extra)
		.await
		.expect_err("null keys count");

	assert_eq!(error.kind(), ErrorKind::ProfileTooLarge);
	assert_eq!(services.globals.current_count(), before);

	let oversized: Response =
		once(("displayname".to_owned(), json!("x".repeat(MAX_PROFILE_SIZE - 20)))).collect();

	let error = service
		.merge_profile(user, oversized)
		.await
		.expect_err("retained null costs bytes");

	assert_eq!(error.kind(), ErrorKind::ProfileTooLarge);
	assert_eq!(services.globals.current_count(), before);
	assert_eq!(field(service, user, "displayname").await?, json!(null));

	let replacement: Response =
		once(("avatar_url".to_owned(), json!("mxc://remote.example/repaired"))).collect();

	service.merge_profile(user, replacement).await?;

	assert_eq!(
		field(service, user, "avatar_url").await?,
		json!("mxc://remote.example/repaired")
	);

	assert_eq!(field(service, user, "displayname").await?, json!(null));
	assert_eq!(services.globals.current_count(), before + 1);

	Ok(())
}
