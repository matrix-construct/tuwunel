use std::array::from_fn;

use tuwunel_core::{Result, config::Figment, itertools::Itertools};

use super::{Summary, checks, find};
use crate::{
	Services,
	migrations::{DATABASE_VERSION, MARKERS, fresh},
	test_utils::fixture,
};

#[test]
fn check_names_are_unique() {
	let names = checks().map(|check| check.name);

	assert_eq!(names.clone().count(), names.sorted_unstable().dedup().count());
}

#[tokio::test]
async fn fresh_database_is_clean() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;

	for check in checks().filter(|check| check.runnable(services)) {
		assert_eq!(count(services, check.name).await?, 0, "{}", check.name);
	}

	Ok(())
}

async fn count(services: &Services, name: &str) -> Result<u64> {
	Ok(run(services, name).await?.findings())
}

async fn run(services: &Services, name: &str) -> Result<Summary> {
	find(name)
		.expect("the check is registered")
		.run(services, 8)
		.await
}

#[tokio::test]
async fn schema_reports_an_unstamped_step() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;
	services.db["global"].remove(MARKERS[0]);

	assert_eq!(count(services, "schema").await?, 1);

	Ok(())
}

#[tokio::test]
async fn schema_reports_a_foreign_version() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;
	services
		.globals
		.db
		.bump_database_version(u64::MAX);

	let summary = run(services, "schema").await?;

	assert_eq!(summary.counts.get("schema-version"), Some(&1));

	Ok(())
}

#[tokio::test]
async fn schema_reports_a_missing_version() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	fresh(services).await?;
	services.db["global"].remove(b"version");

	let summary = run(services, "schema").await?;

	assert_eq!(summary.counts.get("missing-version"), Some(&1));

	Ok(())
}

#[tokio::test]
async fn schema_reports_a_malformed_version() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;

	let overlong = overlong(DATABASE_VERSION);

	fresh(services).await?;

	for version in [&b"bad"[..], &overlong] {
		services.db["global"].insert(b"version", version);

		let summary = run(services, "schema").await?;

		assert_eq!(summary.counts.get("malformed-version"), Some(&1), "{version:?}");
	}

	Ok(())
}

/// An id's eight bytes followed by one more, as no writer stores it.
///
/// Every short id and the schema version are exactly eight bytes, so a check must
/// report this as malformed rather than read its prefix.
fn overlong(id: u64) -> [u8; 9] {
	let bytes = id.to_be_bytes();

	from_fn(|index| bytes.get(index).copied().unwrap_or(0))
}
