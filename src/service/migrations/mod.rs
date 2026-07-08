use std::cmp::{self, Ordering};

use futures::{FutureExt, StreamExt};
use ruma::{
	MilliSecondsSinceUnixEpoch, MxcUri, OwnedRoomId, RoomId, UserId,
	events::room::member::MembershipState,
};
use serde::{Deserialize, de::IgnoredAny};
use tuwunel_core::{
	Err, Result, debug, debug_info, debug_warn, err, info,
	itertools::Itertools,
	matrix::{PduCount, pdu::RawPduId},
	result::NotFound,
	utils,
	utils::{
		BoolExt, ReadyExt,
		stream::{BroadbandExt, TryExpect, TryIgnore},
	},
	warn,
};
use tuwunel_database::{Deserialized, Json, SEP};

use crate::{Services, media, rooms::timeline::bias_count};

mod conduit;
mod moderation;

/// The current schema version.
/// - If database is opened at greater version we reject with error. The
///   software must be updated for backward-incompatible changes.
/// - If database is opened at lesser version we apply migrations up to this.
///   Note that named-feature migrations may also be performed when opening at
///   equal or lesser version. These are expected to be backward-compatible.
pub(crate) const DATABASE_VERSION: u64 = 17;

const SERVER_NAME_KEY: &[u8] = b"server_name";

/// A marker written by a sibling conduwuit-lineage server but never by tuwunel.
/// Its presence identifies a foreign database at a higher schema number even
/// after tuwunel has stamped its own `server_name`, so a database opened by
/// both servers in turn keeps booting rather than being refused as too new.
const FOREIGN_LINEAGE_MARKER: &[u8] = b"populate_userroomid_leftstate_table";

pub(crate) async fn migrations(services: &Services) -> Result {
	if !services.config.database_migrations {
		warn!("Skipping database migrations due to configuration...");
		return Ok(());
	}

	let users_count = services.users.count().await;
	if users_count == 0 {
		return fresh(services).await;
	}

	// Computed before check_server_name backfills SERVER_NAME_KEY, which would
	// otherwise mask a Conduit-lineage database (it carries no foreign marker).
	let foreign_lineage = is_foreign_lineage(services).await;

	check_database_version(services, foreign_lineage).await?;
	check_server_name(services).await?;
	migrate(services, foreign_lineage).await
}

/// Whether the database comes from a foreign (non-tuwunel) lineage: it predates
/// our SERVER_NAME_KEY stamp, or carries a conduwuit-lineage migration marker
/// that persists even after we stamp ours. Must be read before the server_name
/// backfill, which removes the first signal.
async fn is_foreign_lineage(services: &Services) -> bool {
	let global = &services.db["global"];

	global.get(SERVER_NAME_KEY).await.is_not_found()
		|| global.get(FOREIGN_LINEAGE_MARKER).await.is_ok()
}

/// Gate the discovered schema version before migrations and the server_name
/// backfill run. The integer is comparable only within tuwunel's own lineage; a
/// foreign database (Conduit and forks) numbers schema on a colliding ladder
/// and is recognized as foreign by [`is_foreign_lineage`], so its number is not
/// gated. Within our lineage a version below 13 is refused as unmigratable and
/// one above this build as too new to open safely; force_migration overrides
/// the latter for a deliberate downgrade.
async fn check_database_version(services: &Services, foreign_lineage: bool) -> Result {
	let discovered = services.globals.db.database_version().await;

	if discovered < 13 {
		return Err!(Database("Database schema version {discovered} is no longer supported"));
	}

	if discovered > DATABASE_VERSION && !foreign_lineage && !services.config.force_migration {
		return Err!(Database(
			"Database schema version {discovered} is newer than this build supports \
			 ({DATABASE_VERSION}). Upgrade tuwunel, or set force_migration = true to open it \
			 anyway; a downgrade may cause permanent data loss."
		));
	}

	Ok(())
}

/// Matrix resource ownership is based on the server name; changing it
/// requires recreating the database from scratch. The marker is stamped
/// once in fresh(); pre-marker databases are backfilled by probing for
/// any user from the configured server.
async fn check_server_name(services: &Services) -> Result {
	let server_name = &services.server.name;

	let existing = services.db["global"]
		.get(SERVER_NAME_KEY)
		.await
		.deserialized::<String>();

	match existing {
		| Err(_) => backfill_server_name(services).await,
		| Ok(existing) if existing.eq(server_name) => Ok(()),
		| Ok(existing) => Err!(Database(
			"Database belongs to {existing}; configured server name is {server_name}. Cannot \
			 reuse."
		)),
	}
}

/// Stamp the marker on a database that pre-dates SERVER_NAME_KEY by probing
/// for any user from the configured server. If none, the database belongs
/// to a different server and reuse is refused.
async fn backfill_server_name(services: &Services) -> Result {
	let server_name = &services.server.name;

	services
		.users
		.stream()
		.ready_any(|user_id| services.globals.user_is_local(user_id))
		.await
		.into_option()
		.ok_or_else(|| {
			err!(Database(
				"Database has no users from {server_name}; refusing to reuse with this \
				 server_name."
			))
		})?;

	services.db["global"].insert(SERVER_NAME_KEY, server_name.as_str());
	info!(%server_name, "Stamped server_name marker on upgraded database");

	Ok(())
}

async fn fresh(services: &Services) -> Result {
	let db = &services.db;

	services
		.globals
		.db
		.bump_database_version(DATABASE_VERSION);

	db["global"].insert(SERVER_NAME_KEY, services.server.name.as_str());
	db["global"].insert(b"feat_sha256_media", []);
	db["global"].insert(b"fix_pdu_missing_room_id", []);
	db["global"].insert(b"fix_bad_double_separator_in_state_cache", []);
	db["global"].insert(b"retroactively_fix_bad_data_from_roomuserid_joined", []);
	db["global"].insert(b"fix_referencedevents_missing_sep", []);
	db["global"].insert(b"fix_readreceiptid_readreceipt_duplicates", []);
	db["global"].insert(b"fix_hashed_sentinel_passwords", []);
	db["global"].insert(b"upgrade_legacy_mediaid_user", []);
	db["global"].insert(b"remove_remote_media_userid", []);
	db["global"].insert(b"rebuild_roomid_tscount_pducount", []);
	db["global"].insert(b"rebuild_relatesto_typed", []);
	db["global"].insert(b"migrate_profile_keys_to_useridprofilekey", []);
	db["global"].insert(b"rebuild_thread_activity", []);

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
#[expect(clippy::too_many_lines)]
async fn migrate(services: &Services, foreign_lineage: bool) -> Result {
	let db = &services.db;

	let target_version = DATABASE_VERSION;
	let discovered = services.globals.db.database_version().await;

	// Claim our schema version up front when importing a foreign database
	// numbered above ours (e.g. Conduit at 18). Stamping only at the end would
	// leave an aborted import unbootable: the server_name backfill has already
	// run, so a restart no longer sees the database as foreign and the version
	// gate refuses it. The per-step markers below remain the real idempotency
	// gates, so an aborted import still resumes where it left off.
	if foreign_lineage && discovered > target_version {
		services
			.globals
			.db
			.bump_database_version(target_version);
	}

	migrate_media(services).await?;

	if db["global"]
		.get(b"fix_pdu_missing_room_id")
		.await
		.is_not_found()
	{
		conduit::migrate_conduit_pdus(services).await?;
		db["global"].insert(b"fix_pdu_missing_room_id", []);
	}

	import_conduit_knocks(services).await?;
	split_conduit_highlight_counts(services).await?;

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

	if db["global"]
		.get(b"upgrade_legacy_mediaid_user")
		.await
		.is_not_found()
	{
		upgrade_legacy_mediaid_user(services).await?;
	}

	if db["global"]
		.get(b"remove_remote_media_userid")
		.await
		.is_not_found()
	{
		remove_remote_media_userid(services).await?;
	}

	if db["global"]
		.get(b"rebuild_roomid_tscount_pducount")
		.await
		.is_not_found()
	{
		rebuild_roomid_tscount_pducount(services).await?;
	}

	if db["global"]
		.get(b"rebuild_relatesto_typed")
		.await
		.is_not_found()
	{
		services
			.pdu_metadata
			.rebuild_typed_relations()
			.await?;

		db["global"].insert(b"rebuild_relatesto_typed", []);
	}

	if db["global"]
		.get(b"migrate_profile_keys_to_useridprofilekey")
		.await
		.is_not_found()
	{
		migrate_profile_keys(services).await?;
	}

	if db["global"]
		.get(b"rebuild_thread_activity")
		.await
		.is_not_found()
	{
		services.threads.rebuild_thread_activity().await?;

		db["global"].insert(b"rebuild_thread_activity", []);
	}

	// Non-destructive and idempotent, so it runs every boot rather than once: a
	// suspension added by an origin server after a prior tuwunel boot still
	// carries on the next one.
	moderation::migrate_moderation(services).await?;

	// A newer same-lineage database was already refused; stamping ours is safe. A
	// foreign import above our version was already stamped down before the import
	// ran, so this is a no-op for it.
	services
		.globals
		.db
		.bump_database_version(target_version);

	match discovered.cmp(&target_version) {
		| Ordering::Less =>
			info!("Database: migrated schema version from {discovered} to {target_version}."),
		| Ordering::Greater => warn!(
			"Database: stamped schema version {target_version} over a higher discovered version \
			 {discovered} (forced downgrade or foreign import)."
		),
		| Ordering::Equal => {},
	}

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

/// Imports a Conduit database's pending knocks once. Gated on its own marker
/// and the source column's presence, so it runs only for a Conduit database and
/// only the first time; a re-import would resurrect a knock the user later
/// resolved.
async fn import_conduit_knocks(services: &Services) -> Result {
	let db = &services.db;

	let pending = db["global"]
		.get(b"imported_conduit_knocks")
		.await
		.is_not_found();

	if pending && db.open_cf("roomuserid_knockcount")?.is_some() {
		conduit::migrate_conduit_knocks(services).await?;
		db["global"].insert(b"imported_conduit_knocks", []);
	}

	Ok(())
}

/// Splits a Conduit database's conflated highlight-count column once. Conduit
/// aliased `roomuserid_lastnotificationread` onto the
/// `userroomid_highlightcount` tree, so one column holds both stores; tuwunel
/// keeps them apart. Gated on its own marker; the split itself returns early
/// unless a room-keyed row is present, so it is a cheap no-op on a native
/// database.
async fn split_conduit_highlight_counts(services: &Services) -> Result {
	let db = &services.db;

	if db["global"]
		.get(b"split_conduit_highlight")
		.await
		.is_not_found()
	{
		conduit::migrate_conduit_highlight_split(services).await?;
		db["global"].insert(b"split_conduit_highlight", []);
	}

	Ok(())
}

/// Imports a Conduit database's content-addressed media into tuwunel's
/// key-addressed store when it is present and not yet imported; otherwise runs
/// the key-addressed media migrations.
async fn migrate_media(services: &Services) -> Result {
	let db = &services.db;
	let config = &services.server.config;

	let sha256_done = !db["global"]
		.get(b"feat_sha256_media")
		.await
		.is_not_found();

	// The foreign CF persists, so the marker (not its presence) is the latch.
	if !sha256_done
		&& db
			.open_cf("servernamemediaid_metadata")?
			.is_some()
	{
		conduit::migrate_conduit_media(services).await?;
		db["global"].insert(b"feat_sha256_media", []);
		return Ok(());
	}

	if !sha256_done {
		media::migrations::migrate_sha256_media(services).await?;
	} else if config.media_startup_check {
		media::migrations::checkup_sha256_media(services).await?;
	}

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
			let Some(first_sep_index) = key.iter().position(|&i| i == 0xFF) else {
				debug_warn!(?key, "roomuserid_joined key has no 0xFF separator; skipping");
				return;
			};

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

	services
		.metadata
		.iter_ids()
		.for_each(async |room_id| {
			debug_info!(%room_id, "Fixing room");

			services
				.state_cache
				.room_members(room_id)
				.map(ToOwned::to_owned)
				.broad_filter_map(async |user_id| {
					// A member with no resolved member event is left untouched.
					let member = services
						.state_accessor
						.get_member(room_id, &user_id)
						.await
						.ok()?;

					Some((user_id, member.membership))
				})
				.ready_for_each(|(user_id, membership)| {
					let count = services.globals.next_count();

					match membership {
						| MembershipState::Join => services.state_cache.mark_as_joined(
							&user_id,
							room_id,
							PduCount::Normal(*count),
						),
						| _ => services.state_cache.mark_as_left(
							&user_id,
							room_id,
							PduCount::Normal(*count),
						),
					}
				})
				.await;

			services
				.state_cache
				.update_joined_count(room_id)
				.await;
		})
		.await;

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

			let has_sep = key.contains(&SEP);

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

async fn upgrade_legacy_mediaid_user(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let mediaid_user = db["mediaid_user"].clone();

	warn!("Upgrading legacy mediaid_user keys to composite (mxc, user_id) layout");

	let (checked, upgraded, removed_invalid) = mediaid_user
		.raw_stream()
		.ignore_err()
		.ready_fold(
			(0_usize, 0_usize, 0_usize),
			|(mut checked, mut upgraded, mut removed_invalid), (raw_key, raw_val)| {
				checked = checked.saturating_add(1);

				let has_sep = raw_key.contains(&SEP);
				let user_id = str::from_utf8(raw_val)
					.ok()
					.and_then(|s| <&UserId>::try_from(s).ok());

				match (has_sep, user_id) {
					| (true, _) => {},
					| (false, None) => {
						warn!(
							?raw_key,
							?raw_val,
							"Legacy entry has unparsable user_id, removing"
						);

						mediaid_user.remove(raw_key);
						removed_invalid = removed_invalid.saturating_add(1);
					},
					| (false, Some(user_id)) => {
						let mut new_key = raw_key.to_vec();
						new_key.push(SEP);
						new_key.extend_from_slice(user_id.as_bytes());

						mediaid_user.put_raw(new_key, user_id.as_str());
						mediaid_user.remove(raw_key);

						upgraded = upgraded.saturating_add(1);
					},
				}

				(checked, upgraded, removed_invalid)
			},
		)
		.await;

	drop(cork);
	info!(
		%checked,
		%upgraded,
		%removed_invalid,
		"Upgraded legacy mediaid_user keys"
	);

	db["global"].insert(b"upgrade_legacy_mediaid_user", []);
	db.engine.sort()
}

async fn remove_remote_media_userid(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let mediaid_user = db["mediaid_user"].clone();

	warn!("Removing stored user id for remote media");

	let (checked, removed_remote, removed_invalid) = mediaid_user
		.keys()
		.expect_ok()
		.ready_fold(
			(0, 0, 0),
			|(mut checked, mut removed_remote, mut removed_invalid): (usize, usize, usize),
			 (mxc_uri, user_id): (&MxcUri, &UserId)| {
				checked = checked.saturating_add(1);

				let Ok(mxc) = mxc_uri.parts() else {
					warn!(?mxc_uri, "Invalid MXC URL, removing it");

					mediaid_user.del((mxc_uri, user_id));

					removed_invalid = removed_invalid.saturating_add(1);

					return (checked, removed_remote, removed_invalid);
				};

				if !services.globals.server_is_ours(mxc.server_name) {
					mediaid_user.del((mxc_uri, user_id));

					removed_remote = removed_remote.saturating_add(1);

					return (checked, removed_remote, removed_invalid);
				}

				(checked, removed_remote, removed_invalid)
			},
		)
		.await;

	drop(cork);
	info!(
		%checked,
		%removed_remote,
		%removed_invalid,
		"Removed stored user id for remote media"
	);

	db["global"].insert(b"remove_remote_media_userid", []);
	db.engine.sort()
}

#[derive(Deserialize)]
struct PduRoomTs {
	room_id: OwnedRoomId,
	origin_server_ts: MilliSecondsSinceUnixEpoch,
}

async fn rebuild_roomid_tscount_pducount(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let pduid_pdu = db["pduid_pdu"].clone();
	let roomid_tscount_pducount = db["roomid_tscount_pducount"].clone();

	warn!("Rebuilding roomid_tscount_pducount index for same-timestamp event ordering");

	let count = pduid_pdu
		.raw_stream()
		.ignore_err()
		.ready_fold(0_usize, |count, (key, value)| {
			let Ok(pdu) = serde_json::from_slice::<PduRoomTs>(value) else {
				return count;
			};

			let ts = u64::from(pdu.origin_server_ts.get());
			let pdu_id = RawPduId::from(key);
			let count_key = bias_count(pdu_id.count());
			let room_id: &RoomId = &pdu.room_id;

			roomid_tscount_pducount.put_raw((room_id, ts, count_key), pdu_id.count());

			count.saturating_add(1)
		})
		.await;

	drop(cork);
	info!(%count, "Rebuilt roomid_tscount_pducount index");

	db["global"].insert(b"rebuild_roomid_tscount_pducount", []);
	db.engine.sort()
}

/// Relocates the per-user displayname and avatar_url out of their dedicated
/// columns into the unified useridprofilekey_value store keyed by MSC4133 field
/// name, where the profile service now reads them. The dedicated columns are
/// left intact, so an older binary opening the same database still resolves.
async fn migrate_profile_keys(services: &Services) -> Result {
	use ruma::profile::ProfileFieldName;

	let db = &services.db;
	let cork = db.cork_and_sync();

	let userid_displayname = db["userid_displayname"].clone();
	let userid_avatarurl = db["userid_avatarurl"].clone();
	let userid_blurhash = db["userid_blurhash"].clone();
	let useridprofilekey_value = db["useridprofilekey_value"].clone();

	warn!(
		"Relocating displaynames, avatar_urls and blurhashes into the unified profile-key store"
	);

	let displaynames = userid_displayname
		.stream()
		.expect_ok()
		.ready_fold(0_usize, |count, (user_id, displayname): (&UserId, &str)| {
			let key = (user_id, ProfileFieldName::DisplayName.as_str());
			let value = displayname.to_owned();

			useridprofilekey_value.put(key, Json(value));

			count.saturating_add(1)
		})
		.await;

	let avatar_urls = userid_avatarurl
		.stream()
		.expect_ok()
		.ready_fold(0_usize, |count, (user_id, avatar_url): (&UserId, &str)| {
			let key = (user_id, ProfileFieldName::AvatarUrl.as_str());
			let value = avatar_url.to_owned();

			useridprofilekey_value.put(key, Json(value));

			count.saturating_add(1)
		})
		.await;

	let blurhashes = userid_blurhash
		.stream()
		.expect_ok()
		.ready_fold(0_usize, |count, (user_id, blurhash): (&UserId, &str)| {
			let key = (user_id, "xyz.amorgan.blurhash");
			let value = blurhash.to_owned();

			useridprofilekey_value.put(key, Json(value));

			count.saturating_add(1)
		})
		.await;

	let fixed_strings = useridprofilekey_value
		.raw_stream()
		.expect_ok()
		.ready_fold(0_usize, |count, (key, value)| {
			if serde_json::from_slice::<IgnoredAny>(value).is_err() {
				let Ok(string) = str::from_utf8(value) else {
					warn!("Non-UTF8 data in profile value: {key:?} => {value:?}");
					useridprofilekey_value.remove(key);
					return count;
				};
				useridprofilekey_value.raw_put(key, Json(string));
				return count.saturating_add(1);
			}

			count
		})
		.await;

	drop(cork);
	info!(%displaynames, %avatar_urls, %blurhashes, %fixed_strings, "Relocated profile keys into useridprofilekey_value");

	db["global"].insert(b"migrate_profile_keys_to_useridprofilekey", []);
	db.engine.sort()
}
