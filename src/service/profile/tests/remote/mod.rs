use std::iter::once;

use ruma::{
	UserId, api::federation::query::get_profile_information::v1::Response,
	profile::ProfileFieldName, user_id,
};
use serde_json::{Value, json};
use tuwunel_core::{Result, config::Figment, utils::stream::ReadyExt};

use super::super::Service;
use crate::test_utils::fixture;

const KEPT: &str = "com.example.kept";
const STALE: &str = "org.matrix.msc4426.status";

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
