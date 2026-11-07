//! Existing names the configuration forbids.
//!
//! The ladder only warns about them, so they stay reported after it runs.

use ruma::{room_alias_id, room_id};
use tuwunel_core::{Result, config::Figment};

use super::{count, run};
use crate::{
	migrations::{fresh, migrate},
	test_utils::{Warnings, fixture},
	users::PASSWORD_SENTINEL,
};

#[tokio::test]
async fn forbidden_names_stay_reported() -> Result {
	let config = Figment::new()
		.merge(("admin.room.create", false))
		.merge(("registration.forbidden_usernames", ["^bad"]))
		.merge(("rooms.forbidden_alias_names", ["^bad"]));

	let Some(fixture) = fixture(config).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let passwords = &services.db["userid_password"];
	let room = room_id!("!room:localhost");
	let alias = room_alias_id!("#badalias:localhost");

	fresh(services).await?;
	passwords.insert("@badname:localhost", PASSWORD_SENTINEL);
	passwords.insert("@goodname:localhost", PASSWORD_SENTINEL);
	passwords.insert("@badgone:localhost", []);
	passwords.insert("@badremote:example.org", PASSWORD_SENTINEL);
	services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let directory = &services.alias;

	directory.set_alias(alias, room)?;
	directory.set_alias(room_alias_id!("#goodalias:localhost"), room)?;
	directory.set_alias(room_alias_id!("#badorphan:localhost"), room_id!("!orphan:localhost"))?;

	let usernames = run(services, "forbidden-usernames").await?;
	let aliases = run(services, "forbidden-aliases").await?;

	assert_eq!(usernames.counts.get("forbidden-username"), Some(&1));
	assert_eq!(usernames.findings(), 1);
	assert_eq!(aliases.counts.get("forbidden-alias"), Some(&1));
	assert_eq!(aliases.findings(), 1);

	let warnings = Warnings::capture();

	migrate(services, false).await?;

	assert!(warnings.warned("user_id", "@badname:localhost"));
	assert!(warnings.warned("alias", alias.as_str()));
	assert_eq!(count(services, "forbidden-usernames").await?, 1);
	assert_eq!(count(services, "forbidden-aliases").await?, 1);

	Ok(())
}
