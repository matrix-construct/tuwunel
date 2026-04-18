use std::cmp;

use futures::{FutureExt, StreamExt};
use ruma::{OwnedUserId, RoomId, UserId, events::room::member::MembershipState};
use tuwunel_core::{
	Err, Result, debug, debug_info, debug_warn, err, error, info,
	itertools::Itertools,
	matrix::PduCount,
	result::NotFound,
	utils,
	utils::{
		IterStream, ReadyExt,
		stream::{TryExpect, TryIgnore},
	},
	warn,
};

use crate::{Services, media};

/// The current schema version.
/// - If database is opened at greater version we reject with error. The
///   software must be updated for backward-incompatible changes.
/// - If database is opened at lesser version we apply migrations up to this.
///   Note that named-feature migrations may also be performed when opening at
///   equal or lesser version. These are expected to be backward-compatible.
pub(crate) const DATABASE_VERSION: u64 = 17;

pub(crate) async fn migrations(services: &Services) -> Result {
	if !services.config.database_migrations {
		warn!("Skipping database migrations due to configuration...");
		return Ok(());
	}

	let users_count = services.users.count().await;

	// Matrix resource ownership is based on the server name; changing it
	// requires recreating the database from scratch.
	if users_count > 0 {
		let server_user = &services.globals.server_user;
		if !services.users.exists(server_user).await {
			error!("The {server_user} server user does not exist, and the database is not new.");
			return Err!(Database(
				"Cannot reuse an existing database after changing the server name, please \
				 delete the old one first.",
			));
		}
	}

	if users_count > 0 {
		migrate(services).await
	} else {
		fresh(services).await
	}
}

async fn fresh(services: &Services) -> Result {
	let db = &services.db;

	services
		.globals
		.db
		.bump_database_version(DATABASE_VERSION);

	db["global"].insert(b"feat_sha256_media", []);
	db["global"].insert(b"fix_bad_double_separator_in_state_cache", []);
	db["global"].insert(b"retroactively_fix_bad_data_from_roomuserid_joined", []);
	db["global"].insert(b"fix_referencedevents_missing_sep", []);
	db["global"].insert(b"fix_readreceiptid_readreceipt_duplicates", []);
	db["global"].insert(b"fix_hashed_sentinel_passwords", []);

	// Create the admin room and server user on first run
	if services.config.create_admin_room {
		crate::admin::create_admin_room(services)
			.boxed()
			.await?;
	}

	warn!("Created new RocksDB database with version {DATABASE_VERSION}");

	Ok(())
}

/// Apply any migrations
async fn migrate(services: &Services) -> Result {
	let db = &services.db;
	let config = &services.server.config;

	let target_version = DATABASE_VERSION;
	let discovered_version = async || services.globals.db.database_version().await;

	if discovered_version().await < 13 {
		return Err!(Database(
			"Database schema version {} is no longer supported",
			discovered_version().await,
		));
	}

	if db["global"]
		.get(b"feat_sha256_media")
		.await
		.is_not_found()
	{
		media::migrations::migrate_sha256_media(services).await?;
	} else if config.media_startup_check {
		media::migrations::checkup_sha256_media(services).await?;
	}

	if db["global"]
		.get(b"fix_bad_double_separator_in_state_cache")
		.await
		.is_not_found()
	{
		fix_bad_double_separator_in_state_cache(services).await?;
	}

	if db["global"]
		.get(b"retroactively_fix_bad_data_from_roomuserid_joined")
		.await
		.is_not_found()
	{
		retroactively_fix_bad_data_from_roomuserid_joined(services).await?;
	}

	if db["global"]
		.get(b"fix_referencedevents_missing_sep")
		.await
		.is_not_found()
	{
		fix_referencedevents_missing_sep(services).await?;
	}

	if db["global"]
		.get(b"fix_readreceiptid_readreceipt_duplicates")
		.await
		.is_not_found()
	{
		fix_readreceiptid_readreceipt_duplicates(services).await?;
	}

	if db["global"]
		.get(b"fix_hashed_sentinel_passwords")
		.await
		.is_not_found()
	{
		fix_hashed_sentinel_passwords(services).await?;
	}

	if discovered_version().await < target_version {
		services
			.globals
			.db
			.bump_database_version(target_version);

		info!(
			"Database: Migrated schema version from {} to {target_version}",
			discovered_version().await
		);
	} else if discovered_version().await != target_version && config.force_migration {
		services
			.globals
			.db
			.bump_database_version(target_version);

		warn!(
			"Database: Forced migration from schema version {} to {target_version}",
			discovered_version().await,
		);
	}

	assert_eq!(
		target_version,
		discovered_version().await,
		"Failed asserting local database version {} is equal to known latest tuwunel database \
		 version {target_version}",
		discovered_version().await,
	);

	if !services.config.forbidden_usernames.is_empty() {
		services
			.users
			.stream()
			.filter(|user_id| services.users.is_active_local(user_id))
			.ready_filter_map(|user_id| {
				let patterns = &services.config.forbidden_usernames;
				let matches = patterns.matches(user_id.localpart());
				let matched = matches
					.iter()
					.map(|x| &patterns.patterns()[x])
					.join(", ");

				matches
					.matched_any()
					.then_some((user_id, matched))
			})
			.ready_for_each(|(user_id, matched)| {
				warn!("User {user_id} matches forbidden username patterns: {matched:#?}");
			})
			.await;
	}

	if !services.config.forbidden_alias_names.is_empty() {
		services
			.metadata
			.iter_ids()
			.map(|room_id| {
				services
					.alias
					.local_aliases_for_room(room_id)
					.map(move |alias| (room_id, alias))
			})
			.flatten()
			.ready_filter_map(|(room_id, room_alias)| {
				let patterns = &services.config.forbidden_alias_names;
				let matches = patterns.matches(room_alias.alias());
				let matched = matches
					.iter()
					.map(|x| &patterns.patterns()[x])
					.join(", ");

				matches
					.matched_any()
					.then_some((room_id, room_alias, matched))
			})
			.ready_for_each(|(room_id, room_alias, matched)| {
				warn!(
					"Room {room_id} with alias {room_alias} matches the following forbidden \
					 room name patterns: {matched}"
				);
			})
			.boxed()
			.await;
	}

	info!("Loaded RocksDB database with schema version {DATABASE_VERSION}");

	Ok(())
}

async fn fix_bad_double_separator_in_state_cache(services: &Services) -> Result {
	warn!("Fixing bad double separator in state_cache roomuserid_joined");

	let db = &services.db;
	let roomuserid_joined = &db["roomuserid_joined"];
	let _cork = db.cork_and_sync();

	let mut iter_count: usize = 0;
	roomuserid_joined
		.raw_stream()
		.ignore_err()
		.ready_for_each(|(key, value)| {
			let mut key = key.to_vec();
			iter_count = iter_count.saturating_add(1);
			debug_info!(%iter_count);
			let first_sep_index = key
				.iter()
				.position(|&i| i == 0xFF)
				.expect("found 0xFF delim");

			if key
				.iter()
				.get(first_sep_index..=first_sep_index.saturating_add(1))
				.copied()
				.collect_vec()
				== vec![0xFF, 0xFF]
			{
				debug_warn!("Found bad key: {key:?}");
				roomuserid_joined.remove(&key);

				key.remove(first_sep_index);
				debug_warn!("Fixed key: {key:?}");
				roomuserid_joined.insert(&key, value);
			}
		})
		.await;

	db.engine.sort()?;
	db["global"].insert(b"fix_bad_double_separator_in_state_cache", []);

	info!("Finished fixing");
	Ok(())
}

async fn retroactively_fix_bad_data_from_roomuserid_joined(services: &Services) -> Result {
	warn!("Retroactively fixing bad data from broken roomuserid_joined");

	let db = &services.db;
	let _cork = db.cork_and_sync();

	let room_ids = services
		.metadata
		.iter_ids()
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>()
		.await;

	for room_id in &room_ids {
		debug_info!("Fixing room {room_id}");

		let users_in_room: Vec<OwnedUserId> = services
			.state_cache
			.room_members(room_id)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		let joined_members = users_in_room
			.iter()
			.stream()
			.filter(|user_id| {
				services
					.state_accessor
					.get_member(room_id, user_id)
					.map(|member| {
						member.is_ok_and(|member| member.membership == MembershipState::Join)
					})
			})
			.collect::<Vec<_>>()
			.await;

		let non_joined_members = users_in_room
			.iter()
			.stream()
			.filter(|user_id| {
				services
					.state_accessor
					.get_member(room_id, user_id)
					.map(|member| {
						member.is_ok_and(|member| member.membership == MembershipState::Join)
					})
			})
			.collect::<Vec<_>>()
			.await;

		for user_id in &joined_members {
			debug_info!("User is joined, marking as joined");
			let count = services.globals.next_count();
			services
				.state_cache
				.mark_as_joined(user_id, room_id, PduCount::Normal(*count));
		}

		for user_id in &non_joined_members {
			debug_info!("User is left or banned, marking as left");
			let count = services.globals.next_count();
			services
				.state_cache
				.mark_as_left(user_id, room_id, PduCount::Normal(*count));
		}
	}

	for room_id in &room_ids {
		debug_info!(
			"Updating joined count for room {room_id} to fix servers in room after correcting \
			 membership states"
		);

		services
			.state_cache
			.update_joined_count(room_id)
			.await;
	}

	db.engine.sort()?;
	db["global"].insert(b"retroactively_fix_bad_data_from_roomuserid_joined", []);

	info!("Finished fixing");
	Ok(())
}

async fn fix_referencedevents_missing_sep(services: &Services) -> Result {
	warn!("Fixing missing record separator between room_id and event_id in referencedevents");

	let db = &services.db;
	let cork = db.cork_and_sync();

	let referencedevents = db["referencedevents"].clone();

	let totals: (usize, usize) = (0, 0);
	let (total, fixed) = referencedevents
		.raw_stream()
		.expect_ok()
		.enumerate()
		.ready_fold(totals, |mut a, (i, (key, val))| {
			debug_assert!(val.is_empty(), "expected no value");

			let has_sep = key.contains(&tuwunel_database::SEP);

			if !has_sep {
				let key_str = std::str::from_utf8(key).expect("key not utf-8");
				let room_id_len = key_str.find('$').expect("missing '$' in key");
				let (room_id, event_id) = key_str.split_at(room_id_len);
				debug!(?a, "fixing {room_id}, {event_id}");

				let new_key = (room_id, event_id);
				referencedevents.put_raw(new_key, val);
				referencedevents.remove(key);
			}

			a.0 = cmp::max(i, a.0);
			a.1 = a.1.saturating_add((!has_sep).into());
			a
		})
		.await;

	drop(cork);
	info!(?total, ?fixed, "Fixed missing record separators in 'referencedevents'.");

	db["global"].insert(b"fix_referencedevents_missing_sep", []);
	db.engine.sort()
}

async fn fix_readreceiptid_readreceipt_duplicates(services: &Services) -> Result {
	use ruma::identifiers_validation::ID_MAX_BYTES;
	use tuwunel_core::arrayvec::ArrayString;

	type ArrayId = ArrayString<ID_MAX_BYTES>;
	type Key<'a> = (&'a RoomId, u64, &'a UserId);

	warn!("Fixing undeleted entries in readreceiptid_readreceipt...");

	let db = &services.db;
	let cork = db.cork_and_sync();
	let readreceiptid_readreceipt = db["readreceiptid_readreceipt"].clone();

	let mut cur_room: Option<ArrayId> = None;
	let mut cur_user: Option<ArrayId> = None;
	let (mut total, mut fixed): (usize, usize) = (0, 0);
	readreceiptid_readreceipt
		.keys()
		.expect_ok()
		.ready_for_each(|key: Key<'_>| {
			let (room_id, _, user_id) = key;
			let last_room = cur_room.replace(
				room_id
					.as_str()
					.try_into()
					.expect("invalid room_id in database"),
			);

			let last_user = cur_user.replace(
				user_id
					.as_str()
					.try_into()
					.expect("invalid user_id in database"),
			);

			let is_dup = cur_room == last_room && cur_user == last_user;
			if is_dup {
				readreceiptid_readreceipt.del(key);
			}

			fixed = fixed.saturating_add(is_dup.into());
			total = total.saturating_add(1);
		})
		.await;

	drop(cork);
	info!(?total, ?fixed, "Fixed undeleted entries in readreceiptid_readreceipt.");

	db["global"].insert(b"fix_readreceiptid_readreceipt_duplicates", []);
	db.engine.sort()
}

async fn fix_hashed_sentinel_passwords(services: &Services) -> Result {
	use tuwunel_core::utils::hash::verify_password;

	const PASSWORD_SENTINEL: &str = "*";

	if services.config.identity_provider.is_empty() {
		debug!("Skipping sentinel password migration since no SSO IdP configured.");
		return Ok(());
	}

	let db = &services.db;
	let cork = db.cork_and_sync();
	let userid_password = db["userid_password"].clone();
	let hashed_sentinel = utils::hash::password(PASSWORD_SENTINEL).map_err(|e| {
		err!("Could not apply migration: failed to hash sentinel password: {e:?}")
	})?;

	warn!(
		"Fixing occurrences of password-hash {hashed_sentinel:?} generated from \
		 {PASSWORD_SENTINEL:?}"
	);

	let (checked, good, bad) = userid_password
		.stream()
		.expect_ok()
		.ready_fold(
			(0, 0, 0),
			|(mut checked, mut good, mut bad): (usize, usize, usize),
			 (key, val): (&str, &str)| {
				let good_sentinel = val == PASSWORD_SENTINEL;
				let bad_sentinel = !val.is_empty()
					&& !good_sentinel
					&& verify_password(PASSWORD_SENTINEL, val).is_ok();

				checked = checked.saturating_add(usize::from(true));
				good = good.saturating_add(usize::from(good_sentinel));
				bad = bad.saturating_add(usize::from(bad_sentinel));

				if bad_sentinel {
					userid_password.insert(key, PASSWORD_SENTINEL);
				}

				(checked, good, bad)
			},
		)
		.await;

	drop(cork);
	info!(?checked, ?good, ?bad, "Fixed any occurrences of hashed sentinel passwords");

	db["global"].insert(b"fix_hashed_sentinel_passwords", []);
	db.engine.sort()
}
