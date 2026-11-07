//! Account data a foreign database keeps in columns of its own.
//!
//! No service-level test can create a foreign column, so a native column stands
//! in for each, walked by the check and handed to the repair directly. The
//! ladder opens the real column by name, so on this database it finds none and
//! finishes each step at once.

use std::sync::Arc;

use ruma::{DeviceId, UserId, device_id, user_id};
use serde::Serialize;
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::{Map, deserialize_from_slice, serialize_key};

use super::kinds;
use crate::{
	Services,
	integrity::{
		account_status::{Columns, scan as account_status},
		email_bindings::scan as email_bindings,
		token_expiry::scan as token_expiry,
	},
	migrations::{
		ADOPT_ACCOUNT_STATUS, ADOPT_EMAIL_BINDINGS,
		account_status::{adopt_deactivations, adopt_passwordless},
		email_bindings::adopt_email_bindings,
		fresh, marker_present, migrate,
		token_expiry::{
			ADOPT_MARKER, RESTORE_MARKER, Restore, adopt as adopt_token_expiry,
			restore as restore_token_expiry, restore_due,
		},
	},
	test_utils::fixture,
};

const EXPIRES: u64 = 1_600_000_000;

const CLEAN: [&str; 0] = [];

type TokenValue<'a> = (&'a UserId, &'a DeviceId, Option<u64>);

#[tokio::test]
async fn token_expiry_adoption_clears_the_finding() -> Result {
	let config = Figment::new()
		.merge(("admin.room.create", false))
		.merge(("well_known.client", "https://localhost"))
		.merge(("authentication.oidc.native_auth", true));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let origin = &services.db["userid_blurhash"];
	let (alice, bob) = (user_id!("@alice:localhost"), user_id!("@bob:localhost"));
	let (erin, frank) = (user_id!("@erin:localhost"), user_id!("@frank:localhost"));
	let gina = user_id!("@gina:localhost");
	let (first, second) = (device_id!("AAAAAAAAAA"), device_id!("AAAAAAAAAB"));
	let other = device_id!("BBBBBBBBBB");
	let padded_key = [serialize_key((gina, other))?.as_slice(), b"\xFFjunk"].concat();
	let padded_expiry = [EXPIRES.to_be_bytes().as_slice(), b"\xFF"].concat();

	fresh(services).await?;
	services.oauth.get_server()?;

	// Both of alice's devices name one token, so the first adoption decides the second.
	seed_origin(origin, alice, first, EXPIRES);
	seed_origin(origin, alice, second, EXPIRES.saturating_add(1));
	seed_token(services, alice, first, "shared-token", &(alice, first));
	seed_token(services, alice, second, "shared-token", &(alice, first));
	seed_origin(origin, bob, other, EXPIRES);
	seed_token(services, bob, other, "native-token", &(bob, other, None::<u64>));
	seed_origin(origin, user_id!("@carol:localhost"), other, EXPIRES);
	origin.insert(b"not a device", EXPIRES.to_be_bytes());
	origin.put_raw((erin, other), b"bad");
	seed_origin(origin, frank, other, EXPIRES);
	services.db["userdeviceid_token"].put_raw((frank, other), b"\xFE\xFD");

	// A release build reads this row from its leading records, so it is adopted.
	origin.insert(&padded_key, &padded_expiry);
	seed_token(services, gina, other, "padded-token", &(gina, other));

	assert_eq!(kinds(token_expiry(services, origin)).await?, [
		"extra-records",
		"unadopted-expiry",
		"unadopted-expiry",
		"unadopted-expiry",
		"unparsable-device",
		"unparsable-expiry",
		"unparsable-token",
	]);

	assert!(adopt_token_expiry(services, origin).await?);
	assert_eq!(kinds(token_expiry(services, origin)).await?, [
		"extra-records",
		"unparsable-device",
		"unparsable-expiry",
		"unparsable-token",
	]);

	assert_eq!(stored_expiry(services, "shared-token").await?, Some(EXPIRES));
	assert_eq!(stored_expiry(services, "padded-token").await?, Some(EXPIRES));
	assert_eq!(stored_expiry(services, "native-token").await?, None);
	assert!(matches!(restore_due(services)?, Restore::Finished));

	services.db["global"].remove(RESTORE_MARKER);
	services.db["global"].remove(ADOPT_MARKER);
	migrate(services, false).await?;

	assert!(marker_present(services, RESTORE_MARKER).await?);
	assert!(marker_present(services, ADOPT_MARKER).await?);

	Ok(())
}

#[tokio::test]
async fn token_expiry_waits_for_a_provider() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let origin = &services.db["userid_blurhash"];
	let (alice, bob) = (user_id!("@alice:localhost"), user_id!("@bob:localhost"));
	let device = device_id!("AAAAAAAAAA");

	fresh(services).await?;
	assert!(services.oauth.get_server().is_err());
	seed_origin(origin, alice, device, EXPIRES);
	seed_token(services, alice, device, "foreign-token", &(alice, device));
	seed_origin(origin, bob, device, EXPIRES);
	seed_token(services, bob, device, "stamped-token", &(bob, device, Some(EXPIRES)));

	assert_eq!(kinds(token_expiry(services, origin)).await?, [
		"unrefreshable-expiry",
		"waiting-for-provider",
	]);

	// Unfinished, so the ladder leaves the adoption's marker unstamped.
	assert!(!adopt_token_expiry(services, origin).await?);
	assert_eq!(stored_expiry(services, "foreign-token").await?, None);
	assert!(matches!(restore_due(services)?, Restore::Finished));

	restore_token_expiry(services, origin).await?;

	assert_eq!(kinds(token_expiry(services, origin)).await?, [
		"waiting-for-provider",
		"waiting-for-provider",
	]);

	assert!(!adopt_token_expiry(services, origin).await?);
	assert_eq!(stored_expiry(services, "stamped-token").await?, None);

	let stored = services.db["token_userdeviceid"]
		.get("stamped-token")
		.await?;

	assert_eq!(&*stored, b"@bob:localhost\xFFAAAAAAAAAA");

	Ok(())
}

fn seed_origin(origin: &Arc<Map>, user_id: &UserId, device_id: &DeviceId, expires: u64) {
	origin.put_raw((user_id, device_id), expires.to_be_bytes());
}

fn seed_token<V>(
	services: &Services,
	user_id: &UserId,
	device_id: &DeviceId,
	token: &str,
	value: &V,
) where
	V: Serialize,
{
	services.db["userdeviceid_token"].put_raw((user_id, device_id), token);
	services.db["token_userdeviceid"].raw_put(token, value);
}

async fn stored_expiry(services: &Services, token: &str) -> Result<Option<u64>> {
	let stored = services.db["token_userdeviceid"]
		.get(token)
		.await?;

	let (.., expires): TokenValue<'_> = deserialize_from_slice(&stored)?;

	Ok(expires)
}

#[tokio::test]
async fn account_status_adoption_clears_the_finding() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let passwords = &services.db["userid_password"];
	let (deactivated, subjects) =
		(&services.db["userid_blurhash"], &services.db["userid_avatarurl"]);

	let columns = Columns {
		deactivated: Some(deactivated.clone()),
		subjects: Some(subjects.clone()),
	};

	fresh(services).await?;
	services.db["global"].remove(ADOPT_ACCOUNT_STATUS);

	for (user_id, password) in [
		("@alice:localhost", "hash"),
		("@bob:localhost", ""),
		("@carol:localhost", ""),
		("@dave:localhost", ""),
		("@erin:localhost", "hash"),
		("@henry:localhost", "hash"),
	] {
		passwords.insert(user_id, password);
	}

	// A release build reads this key from its leading record, so it is adopted.
	deactivated.insert(b"@henry:localhost\xFFjunk", []);

	for marked in ["@alice:localhost", "@bob:localhost", "@dave:localhost", "@nobody:localhost"] {
		deactivated.insert(marked, []);
	}

	for (subject, localpart) in [
		("carol-sub", "carol"),
		("dave-sub", "dave"),
		("erin-sub", "erin"),
		("nul-sub", "nul\0localpart"),
	] {
		subjects.insert(subject, localpart);
	}

	assert_eq!(kinds(account_status(services, &columns)).await?, [
		"extra-records",
		"unadopted-deactivation",
		"unadopted-deactivation",
		"unadopted-passwordless",
		"unusable-localpart",
	]);

	adopt_deactivations(services, deactivated).await?;
	adopt_passwordless(services, subjects, Some(deactivated)).await?;

	assert_eq!(kinds(account_status(services, &columns)).await?, [
		"extra-records",
		"unusable-localpart"
	]);
	assert_eq!(&*passwords.get("@alice:localhost").await?, b"");
	assert_eq!(&*passwords.get("@henry:localhost").await?, b"");
	assert_eq!(&*passwords.get("@carol:localhost").await?, b"*");
	assert_eq!(&*passwords.get("@dave:localhost").await?, b"");
	assert_eq!(&*passwords.get("@erin:localhost").await?, b"hash");

	// Rows the adoption cannot read fail it, so they are seeded after it ran.
	deactivated.insert(b"not a user", []);
	subjects.insert(b"\xFE", "alice");

	assert_eq!(kinds(account_status(services, &columns)).await?, [
		"extra-records",
		"unparsable-account",
		"unparsable-subject",
		"unusable-localpart",
	]);

	migrate(services, false).await?;

	assert!(marker_present(services, ADOPT_ACCOUNT_STATUS).await?);
	assert_eq!(kinds(account_status(services, &columns)).await?, CLEAN);

	Ok(())
}

#[tokio::test]
async fn email_binding_adoption_reads_its_own_writes() -> Result {
	let config = Figment::new().merge(("admin.room.create", false));
	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let origin = &services.db["userid_blurhash"];
	let owners = &services.db["email_userid"];
	let alice = user_id!("@alice:localhost");

	fresh(services).await?;
	services.db["global"].remove(ADOPT_EMAIL_BINDINGS);

	for user_id in [
		"@alice:localhost",
		"@bob:localhost",
		"@dave:localhost",
		"@erin:localhost",
		"@ivy:localhost",
	] {
		services.db["userid_password"].insert(user_id, "hash");
	}

	// Both of these fold onto one canonical address, which the first in key order takes.
	origin.insert("alice", "Fold@Example.org");
	origin.insert("bob", "fold@example.org");
	origin.insert("carol", "carol@example.org");
	origin.insert("dave", "not an address");
	origin.insert("erin", "erin@example.org");
	origin.insert("nul\0localpart", "nul@example.org");
	owners.insert("erin@example.org", "@frank:localhost");

	// A release build reads this key from its leading record, so it is adopted.
	origin.insert(b"ivy\xFFjunk", "ivy@example.org");

	assert_eq!(kinds(email_bindings(services, origin)).await?, [
		"claimed-elsewhere",
		"extra-records",
		"unadopted-binding",
		"unadopted-binding",
		"unadopted-binding",
		"unusable-address",
		"unusable-localpart",
	]);

	adopt_email_bindings(services, origin).await?;

	assert_eq!(kinds(email_bindings(services, origin)).await?, [
		"claimed-elsewhere",
		"claimed-elsewhere",
		"extra-records",
		"unusable-address",
		"unusable-localpart",
	]);

	let threepid = &services.threepid;
	let bound = threepid
		.user_id_for_email("fold@example.org")
		.await?;

	let padded = threepid
		.user_id_for_email("ivy@example.org")
		.await?;

	assert_eq!(bound.as_deref(), Some(alice));
	assert_eq!(padded.as_deref(), Some(user_id!("@ivy:localhost")));

	// Rows the adoption cannot read fail it, so they are seeded after it ran.
	origin.insert(b"\xFE", "junk@example.org");
	owners.insert("carol@example.org", b"\xFF");
	services.db["userid_password"].insert("@carol:localhost", "hash");

	assert_eq!(kinds(email_bindings(services, origin)).await?, [
		"claimed-elsewhere",
		"claimed-elsewhere",
		"extra-records",
		"unparsable-binding",
		"unparsable-owner",
		"unusable-address",
		"unusable-localpart",
	]);

	migrate(services, false).await?;

	assert!(marker_present(services, ADOPT_EMAIL_BINDINGS).await?);
	assert_eq!(kinds(email_bindings(services, origin)).await?, CLEAN);

	Ok(())
}
