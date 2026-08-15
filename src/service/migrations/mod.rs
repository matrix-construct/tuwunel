//! One-time database migrations.
//!
//! A fresh database is stamped current and a legacy database is walked
//! through the named migrations, once the version and server name gates
//! decide it is safe to touch.

use std::{cmp::Ordering, time::Duration};

use futures::{FutureExt, StreamExt};
use tokio::time::sleep;
use tuwunel_core::{
	Err, Result, err, info,
	itertools::Itertools,
	result::NotFound,
	utils::{BoolExt, ReadyExt},
	warn,
};
use tuwunel_database::Deserialized;

use self::{
	clear_servername_status::clear_servername_status,
	fix_bad_double_separator_in_state_cache::fix_bad_double_separator_in_state_cache,
	fix_hashed_sentinel_passwords::fix_hashed_sentinel_passwords,
	fix_readreceiptid_readreceipt_duplicates::fix_readreceiptid_readreceipt_duplicates,
	fix_referencedevents_missing_sep::fix_referencedevents_missing_sep,
	import_conduit_knocks::import_conduit_knocks,
	injectivity::{fix as fix_injectivity, mark_clean as mark_clean_injectivity},
	migrate_media::migrate_media,
	migrate_profile_keys::migrate_profile_keys,
	rebuild_roomid_tscount_pducount::rebuild_roomid_tscount_pducount,
	remove_remote_media_userid::remove_remote_media_userid,
	retroactively_fix_bad_data_from_roomuserid_joined::retroactively_fix_bad_data_from_roomuserid_joined,
	split_conduit_highlight_counts::split_conduit_highlight_counts,
	upgrade_legacy_mediaid_user::upgrade_legacy_mediaid_user,
};
use crate::Services;

mod clear_servername_status;
mod conduit;
mod fix_bad_double_separator_in_state_cache;
mod fix_hashed_sentinel_passwords;
mod fix_readreceiptid_readreceipt_duplicates;
mod fix_referencedevents_missing_sep;
mod import_conduit_knocks;
mod injectivity;
mod migrate_media;
mod migrate_profile_keys;
mod moderation;
mod rebuild_roomid_tscount_pducount;
mod remove_remote_media_userid;
mod retroactively_fix_bad_data_from_roomuserid_joined;
mod split_conduit_highlight_counts;
mod upgrade_legacy_mediaid_user;

/// The current schema version.
/// - If database is opened at greater version we reject with error. The
///   software must be updated for backward-incompatible changes.
/// - If database is opened at lesser version we apply migrations up to this.
///   Note that named-feature migrations may also be performed when opening at
///   equal or lesser version. These are expected to be backward-compatible.
pub(crate) const DATABASE_VERSION: u64 = 17;

const SERVER_NAME_KEY: &[u8] = b"server_name";

const FORCE_MIGRATION_DELAY: Duration = Duration::from_secs(15);

/// A marker written by a sibling conduwuit-lineage server but never by tuwunel.
/// Its presence identifies a foreign database at a higher schema number even
/// after tuwunel has stamped its own `server_name`, so a database opened by
/// both servers in turn keeps booting rather than being refused as too new.
const FOREIGN_LINEAGE_MARKER: &[u8] = b"populate_userroomid_leftstate_table";

pub(crate) async fn migrations(services: &Services) -> Result {
	if services.config.force_migration {
		warn!(
			delay = ?FORCE_MIGRATION_DELAY,
			"The force_migration option is set. THIS IS NOT INTENDED TO BE USED UNDER ANY \
			 NORMAL CIRCUMSTANCES AND YOU MAY BE CORRUPTING YOUR DATABASE BY PROCEEDING. \
			 Remove force_migration from the configuration to clear this warning; startup \
			 continues after the delay."
		);

		sleep(FORCE_MIGRATION_DELAY).await;
	}

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

	// Repairs residue rather than the schema, so it sits behind the gates
	// that can still refuse this database.
	fix_injectivity(services).await?;

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
			 ({DATABASE_VERSION}). Upgrade tuwunel to a build supporting this database."
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
	db["global"].insert(b"clear_servername_status", []);
	mark_clean_injectivity(services);

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

	// The next two repairs fix a conduwuit-era roomuserid_joined bug Conduit
	// never had; record them done for a Conduit database instead of running.
	if db
		.open_cf("servernamemediaid_metadata")?
		.is_some()
	{
		db["global"].insert(b"fix_bad_double_separator_in_state_cache", []);
		db["global"].insert(b"retroactively_fix_bad_data_from_roomuserid_joined", []);
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

	if db["global"]
		.get(b"clear_servername_status")
		.await
		.is_not_found()
	{
		clear_servername_status(services).await?;
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
		| Ordering::Less => {
			info!("Database: migrated schema version from {discovered} to {target_version}.");
		},
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
