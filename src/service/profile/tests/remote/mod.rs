use std::iter::once;

use ruma::{
	UserId, api::federation::query::get_profile_information::v1::Response,
	profile::ProfileFieldName, room_id, user_id,
};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment, utils::stream::ReadyExt};
use tuwunel_database::Deserialized;

use super::super::{MAX_PROFILE_SIZE, Service};
use crate::test_utils::fixture;

const KEPT: &str = "com.example.kept";
const STALE: &str = "org.matrix.msc4426.status";

#[tokio::test]
async fn selected_fields_bound_raw_count_and_name_bytes() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.profile;
	let mut fields = vec![ProfileFieldName::DisplayName; 64];

	service.check_requested_fields(&fields)?;
	fields.push(ProfileFieldName::DisplayName);
	service
		.check_requested_fields(&fields)
		.expect_err("raw duplicates count before normalization");

	service.check_requested_fields(&["a".repeat(255).into()])?;
	service
		.check_requested_fields(&["a".repeat(256).into()])
		.expect_err("selector names are bounded in bytes");

	service
		.check_requested_fields(&["é".repeat(128).into()])
		.expect_err("a short character count can exceed the byte limit");

	Ok(())
}

#[tokio::test]
async fn merge_counts_canonical_fields_at_the_default_boundary() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let user = user_id!("@counted:remote.example");
	let response: Response = (0..98)
		.map(|index| (format!("com.example.field{index}"), json!(null)))
		.chain([
			("displayname".to_owned(), json!("Remote")),
			("avatar_url".to_owned(), json!(null)),
		])
		.collect();

	services
		.profile
		.merge_profile(user, response)
		.await?;

	let before = services.globals.current_count();
	let extra: Response = once((KEPT.to_owned(), json!("excess"))).collect();

	services
		.profile
		.merge_profile(user, extra)
		.await
		.expect_err("canonical fields count toward the retained limit");

	assert!(
		field(&services.profile, user, KEPT)
			.await
			.is_err_and(|error| error.is_not_found())
	);

	assert_eq!(services.globals.current_count(), before);

	Ok(())
}

#[tokio::test]
async fn remote_count_override_bounds_merge_and_mirror() -> Result {
	let config = Figment::new().merge(("max_remote_profile_fields", 2));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let user = user_id!("@limited:remote.example");
	let response: Response = [
		("displayname".to_owned(), json!("Remote")),
		("avatar_url".to_owned(), json!(null)),
		(KEPT.to_owned(), json!("excess")),
	]
	.into_iter()
	.collect();

	services
		.profile
		.merge_profile(user, response.clone())
		.await
		.expect_err("over-limit response must be rejected before user creation");

	assert!(!services.users.exists(user).await);

	services
		.profile
		.mirror_profile(user, response)
		.await
		.expect_err("explicit replacement must obey the remote count limit");

	let response: Response = [(KEPT.to_owned(), json!("kept")), (STALE.to_owned(), json!(null))]
		.into_iter()
		.collect();

	services
		.profile
		.merge_profile(user, response)
		.await?;

	assert_eq!(field(&services.profile, user, KEPT).await?, json!("kept"));
	assert_eq!(field(&services.profile, user, STALE).await?, json!(null));

	Ok(())
}

#[tokio::test]
async fn merge_rejects_invalid_profiles_before_creating_users() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let user = user_id!("@rejected:remote.example");
	let responses: [Response; 2] = [
		once(("Com.Example.Invalid".to_owned(), json!("new"))).collect(),
		once((KEPT.to_owned(), json!("x".repeat(MAX_PROFILE_SIZE)))).collect(),
	];

	for response in responses {
		services
			.profile
			.merge_profile(user, response)
			.await
			.expect_err("invalid profile");

		assert!(!services.users.exists(user).await);
		assert!(
			field(&services.profile, user, KEPT)
				.await
				.is_err_and(|error| error.is_not_found())
		);
	}

	Ok(())
}

#[tokio::test]
async fn merge_bounds_the_retained_union_without_deleting_omissions() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.profile;
	let user = user_id!("@merged:remote.example");
	let kept = json!("x".repeat(MAX_PROFILE_SIZE / 2));
	let first: Response = once((KEPT.to_owned(), kept.clone())).collect();

	service.merge_profile(user, first).await?;

	let before = fixture.services.globals.current_count();
	let excess: Response = once(("com.example.extra".to_owned(), kept.clone())).collect();

	service
		.merge_profile(user, excess)
		.await
		.expect_err("retained union exceeds size");

	assert_eq!(field(service, user, KEPT).await?, kept);
	assert!(
		field(service, user, "com.example.extra")
			.await
			.is_err_and(|error| error.is_not_found())
	);

	assert_eq!(fixture.services.globals.current_count(), before);

	service
		.merge_profile(user, Response::default())
		.await?;

	assert_eq!(field(service, user, KEPT).await?, kept);
	assert_eq!(fixture.services.globals.current_count(), before);

	Ok(())
}

#[tokio::test]
async fn merge_publishes_one_count_and_skips_unchanged_logs() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let user = user_id!("@batch:remote.example");
	let room = room_id!("!joined:localhost");

	services.users.create(user, None, None).await?;
	services.db["userroomid_joined"].put((user, room), 1_u64);

	let before = services.globals.current_count();
	let response: Response = [(KEPT.to_owned(), json!("new")), (STALE.to_owned(), json!(null))]
		.into_iter()
		.collect();

	services
		.profile
		.merge_profile(user, response.clone())
		.await?;

	let count = services.globals.current_count();

	assert_eq!(count, before + 1);

	for scope in [user.as_str(), room.as_str()] {
		for name in [KEPT, STALE] {
			let owner: String = services
				.profile
				.profilechangeid_userid
				.qry(&(scope, count, name))
				.await?
				.deserialized()?;

			assert_eq!(owner, user.as_str());
		}
	}

	services
		.profile
		.merge_profile(user, response)
		.await?;

	assert_eq!(services.globals.current_count(), count);

	Ok(())
}

#[tokio::test]
async fn mirror_removes_fields_the_response_omits() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let service = &fixture.services.profile;
	let user_id = user_id!("@nyx:remote.example");
	let status = json!({ "emoji": "", "text": "meow" });

	service
		.set_profile_keys(
			user_id,
			&[(KEPT.into(), Some(json!("old"))), (STALE.into(), Some(status))],
			None,
		)
		.await?;

	let before = fixture.services.globals.current_count();
	let removed = service.mirror_profile(user_id, served()).await?;

	assert_eq!(removed.as_slice(), [ProfileFieldName::from(STALE)]);
	assert_eq!(field(service, user_id, KEPT).await?, json!("new"));
	assert!(
		field(service, user_id, STALE)
			.await
			.is_err_and(|error| error.is_not_found())
	);

	let logged = service
		.profile_changed(user_id, before, None)
		.ready_any(|(_, name)| name == STALE)
		.await;

	assert!(logged);

	let removed = service.mirror_profile(user_id, served()).await?;

	assert!(removed.is_empty());

	Ok(())
}

fn served() -> Response { once((KEPT.to_owned(), json!("new"))).collect() }

async fn field(service: &Service, user_id: &UserId, name: &str) -> Result<Value> {
	service.profile_key(user_id, &name.into()).await
}
