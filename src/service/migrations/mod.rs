//! One-time database migrations.
//!
//! A fresh database is stamped current and a legacy database is walked
//! through the named migrations, once the version and server name gates
//! decide it is safe to touch.

use std::{cmp::Ordering, time::Duration};

use futures::{FutureExt, StreamExt, TryStreamExt};
use ruma::{OwnedUserId, RoomId, ServerName, UserId};
use tokio::time::sleep;
use tuwunel_core::{
	Err, Result, err, format_small_string, info,
	result::NotFound,
	smallstr::SmallString,
	utils::{BoolExt, ReadyExt, TryReadyExt},
	warn,
};
use tuwunel_database::Deserialized;

use self::{
	account_status::migrate_account_status,
	clear_servername_status::clear_servername_status,
	clear_state_local_error_memos::clear_state_local_error_memos,
	conduit::migrate_pdus,
	email_bindings::migrate_email_bindings,
	fix_bad_double_separator_in_state_cache::fix_bad_double_separator_in_state_cache,
	fix_hashed_sentinel_passwords::fix_hashed_sentinel_passwords,
	fix_readreceiptid_readreceipt_duplicates::fix_readreceiptid_readreceipt_duplicates,
	fix_referencedevents_missing_sep::fix_referencedevents_missing_sep,
	import_conduit_knocks::import_conduit_knocks,
	injectivity::{fix as fix_injectivity, mark_clean as mark_clean_injectivity},
	migrate_media::migrate_media,
	migrate_profile_keys::migrate_profile_keys,
	moderation::migrate as migrate_moderation,
	rebuild_roomid_tscount_pducount::rebuild_roomid_tscount_pducount,
	redactions::restore_redactions,
	remove_remote_media_userid::remove_remote_media_userid,
	retroactively_fix_bad_data_from_roomuserid_joined::retroactively_fix_bad_data_from_roomuserid_joined,
	split_conduit_highlight_counts::split_conduit_highlight_counts,
	token_expiry::{migrate_token_expiry, restore_token_expiry},
	upgrade_legacy_mediaid_user::upgrade_legacy_mediaid_user,
};
use crate::{
	Services,
	integrity::forbidden_names::{
		Username, matched as forbidden_patterns, usernames as forbidden_usernames,
	},
};

mod account_status;
mod clear_servername_status;
mod clear_state_local_error_memos;
mod conduit;
mod email_bindings;
mod fix_bad_double_separator_in_state_cache;
mod fix_hashed_sentinel_passwords;
mod fix_readreceiptid_readreceipt_duplicates;
mod fix_referencedevents_missing_sep;
mod import_conduit_knocks;
pub(crate) mod injectivity;
mod migrate_media;
mod migrate_profile_keys;
mod moderation;
mod rebuild_roomid_tscount_pducount;
mod redactions;
mod remove_remote_media_userid;
mod retroactively_fix_bad_data_from_roomuserid_joined;
mod split_conduit_highlight_counts;
mod tally;
mod token_expiry;
mod upgrade_legacy_mediaid_user;

#[cfg(test)]
mod tests;

/// The current schema version.
/// - If database is opened at greater version we reject with error. The
///   software must be updated for backward-incompatible changes.
/// - If database is opened at lesser version we apply migrations up to this.
///   Note that named-feature migrations may also be performed when opening at
///   equal or lesser version. These are expected to be backward-compatible.
pub(crate) const DATABASE_VERSION: u64 = 17;

const SERVER_NAME_KEY: &[u8] = b"server_name";

const FORCE_MIGRATION_DELAY: Duration = Duration::from_secs(15);

const CLEAR_STATE_LOCAL_ERROR_MEMOS: &str = "clear_state_local_error_memos";

const REBUILD_LIVE_THREAD_ACTIVITY: &str = "rebuild_live_thread_activity";

/// A marker written by a sibling conduwuit-lineage server but never by tuwunel.
/// Its presence identifies a foreign database at a higher schema number even
/// after tuwunel has stamped its own `server_name`, so a database opened by
/// both servers in turn keeps booting rather than being refused as too new.
const FOREIGN_LINEAGE_MARKER: &[u8] = b"populate_userroomid_leftstate_table";

/// Inline budget for a local user id assembled from a foreign localpart.
type UserIdBuf = SmallString<[u8; 48]>;

/// The steps every database records as settled, whether they ran or it began
/// current.
///
/// Stamped together on a fresh database, so this list is also what an integrity
/// check of the schema expects to find. A step gated on a foreign source has no
/// place here, because a native database never runs it.
pub(crate) const MARKERS: [&str; 21] = [
	"feat_sha256_media",
	"fix_pdu_missing_room_id",
	"fix_bad_double_separator_in_state_cache",
	"retroactively_fix_bad_data_from_roomuserid_joined",
	"fix_referencedevents_missing_sep",
	"fix_readreceiptid_readreceipt_duplicates",
	"fix_hashed_sentinel_passwords",
	"upgrade_legacy_mediaid_user",
	"remove_remote_media_userid",
	"rebuild_roomid_tscount_pducount",
	"rebuild_relatesto_typed",
	"migrate_profile_keys_to_useridprofilekey",
	"rebuild_thread_summaries",
	"clear_servername_status",
	CLEAR_STATE_LOCAL_ERROR_MEMOS,
	"adopt_foreign_account_status",
	"adopt_foreign_email_bindings",
	token_expiry::RESTORE_MARKER,
	token_expiry::ADOPT_MARKER,
	redactions::MARKER,
	REBUILD_LIVE_THREAD_ACTIVITY,
];

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
		if !marker_present(services, CLEAR_STATE_LOCAL_ERROR_MEMOS).await? {
			return Err!(Config(
				"database_migrations",
				"Local state memo invalidation is pending. Enable database_migrations for one \
				 startup, then this setting may be disabled again."
			));
		}

		warn!("Skipping database migrations due to configuration...");
		return Ok(());
	}

	// A stop before any step ran leaves nothing to resume from.
	services.server.check_running()?;

	let users_count = services.users.count().await;
	if users_count == 0 {
		return fresh(services).await;
	}

	// Computed before check_server_name backfills SERVER_NAME_KEY, which would
	// otherwise mask a Conduit-lineage database (it carries no foreign marker).
	let foreign_lineage = is_foreign_lineage(services).await;

	check_database_version(services, foreign_lineage).await?;
	check_server_name(services).await?;

	services.server.check_running()?;

	// Repairs residue rather than the schema, so it sits behind the gates
	// that can still refuse this database.
	fix_injectivity(services).await;

	let migrated = migrate(services, foreign_lineage).await;

	services.server.progress.end();

	migrated.inspect_err(|error| {
		if error.is_interrupted() {
			warn!(
				"Stopped during database migrations. The steps that completed are recorded; the \
				 rest run on the next start."
			);
		}
	})
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

pub(crate) async fn fresh(services: &Services) -> Result {
	let db = &services.db;

	services
		.globals
		.db
		.bump_database_version(DATABASE_VERSION);

	db["global"].insert(SERVER_NAME_KEY, services.server.name.as_str());
	for marker in MARKERS {
		db["global"].insert(marker, []);
	}

	// Older versions rebuild the thread index under this marker.
	db["global"].insert("rebuild_thread_activity", []);
	mark_clean_injectivity(services);

	// Create the admin room and server user on first run
	if services.config.admin.room.create {
		crate::admin::create_admin_room(services)
			.boxed()
			.await?;
	}

	warn!("Created new RocksDB database with version {DATABASE_VERSION}");

	Ok(())
}

/// Apply any migrations
pub(crate) async fn migrate(services: &Services, foreign_lineage: bool) -> Result {
	let db = &services.db;

	services.server.check_running()?;

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

	run_once(services, "fix_pdu_missing_room_id", migrate_pdus).await?;

	import_conduit_knocks(services).await?;

	run_once(services, "split_conduit_highlight", split_conduit_highlight_counts).await?;

	// The next two repairs fix a conduwuit-era roomuserid_joined bug Conduit
	// never had; record them done for a Conduit database instead of running.
	if db
		.open_cf("servernamemediaid_metadata")?
		.is_some()
	{
		db["global"].insert("fix_bad_double_separator_in_state_cache", []);
		db["global"].insert("retroactively_fix_bad_data_from_roomuserid_joined", []);
	}

	run_once(
		services,
		"fix_bad_double_separator_in_state_cache",
		fix_bad_double_separator_in_state_cache,
	)
	.await?;

	run_once(
		services,
		"retroactively_fix_bad_data_from_roomuserid_joined",
		retroactively_fix_bad_data_from_roomuserid_joined,
	)
	.await?;

	run_once(services, "fix_referencedevents_missing_sep", fix_referencedevents_missing_sep)
		.await?;

	run_once(
		services,
		"fix_readreceiptid_readreceipt_duplicates",
		fix_readreceiptid_readreceipt_duplicates,
	)
	.await?;

	until_finished(services, "fix_hashed_sentinel_passwords", fix_hashed_sentinel_passwords)
		.await?;

	run_once(services, "upgrade_legacy_mediaid_user", upgrade_legacy_mediaid_user).await?;
	run_once(services, "remove_remote_media_userid", remove_remote_media_userid).await?;

	run_once(services, "rebuild_roomid_tscount_pducount", rebuild_roomid_tscount_pducount)
		.await?;

	run_once(services, "rebuild_relatesto_typed", async |services: &Services| {
		services
			.pdu_metadata
			.rebuild_typed_relations()
			.await
	})
	.await?;

	run_once(services, "migrate_profile_keys_to_useridprofilekey", migrate_profile_keys).await?;

	run_once(services, "rebuild_thread_summaries", async |services: &Services| {
		rebuild_thread_summaries(services).await;

		Ok(())
	})
	.await?;

	if pending(services, "clear_servername_status").await? {
		clear_servername_status(services).await?;
	}

	if pending(services, CLEAR_STATE_LOCAL_ERROR_MEMOS).await? {
		clear_state_local_error_memos(services).await?;
	}

	services.server.check_running()?;

	// Non-destructive and idempotent, so it runs every boot rather than once: a
	// suspension added by an origin server after a prior tuwunel boot still
	// carries on the next one.
	services
		.server
		.progress
		.begin("migrate_moderation");

	migrate_moderation(services).await?;

	if pending(services, "adopt_foreign_account_status").await? {
		migrate_account_status(services).await?;

		db["global"].insert("adopt_foreign_account_status", []);
	}

	if pending(services, "adopt_foreign_email_bindings").await? {
		migrate_email_bindings(services).await?;

		db["global"].insert("adopt_foreign_email_bindings", []);
	}

	// The restore hands adopted rows back to the adoption, so it runs first.
	until_finished(services, token_expiry::RESTORE_MARKER, restore_token_expiry).await?;
	until_finished(services, token_expiry::ADOPT_MARKER, migrate_token_expiry).await?;

	run_once(services, redactions::MARKER, restore_redactions).await?;

	// Runs after the summary and redaction passes, which can empty a thread.
	run_once(services, REBUILD_LIVE_THREAD_ACTIVITY, async |services: &Services| {
		services.threads.rebuild_thread_activity().await
	})
	.await?;

	services.server.check_running()?;

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

	warn_forbidden_names(services).await?;

	info!("Loaded RocksDB database with schema version {DATABASE_VERSION}");

	Ok(())
}

/// Rebuilds every stored thread summary, logging how many roots changed.
///
/// Failed roots are logged rather than returned, so a caller stamps its marker
/// either way.
async fn rebuild_thread_summaries(services: &Services) {
	let (changed, failed) = services.threads.rebuild_thread_summaries().await;

	if failed == 0 {
		info!(roots = changed, "Rebuilt thread summaries.");
	} else {
		warn!(roots = changed, failed, "Rebuilt thread summaries with failed roots.");
	}
}

/// Runs a pass whose work may wait on a condition outside the database.
///
/// The marker is stamped only once the pass reports it finished, so a pass that
/// waits runs again on a later boot.
async fn until_finished<F>(services: &Services, marker: &'static str, pass: F) -> Result
where
	F: AsyncFnOnce(&Services) -> Result<bool>,
{
	if pending(services, marker).await? {
		let finished = pass(services).await?;

		if finished {
			services.db["global"].insert(marker, []);
		}
	}

	Ok(())
}

/// Runs a pass that finishes whenever it succeeds, stamping its marker.
///
/// A pass that fails leaves the marker unstamped, so the next boot runs it again.
async fn run_once<F>(services: &Services, marker: &'static str, pass: F) -> Result
where
	F: AsyncFnOnce(&Services) -> Result,
{
	if pending(services, marker).await? {
		pass(services).await?;

		services.db["global"].insert(marker, []);
	}

	Ok(())
}

/// Warns about existing names the configuration now forbids.
///
/// The patterns are advisory rather than enforced retroactively, so a match
/// only names the user or the alias in the log, as the forbidden name checks
/// report them. The aliases are walked room by room, as listed in
/// `roomid_shortroomid`, with no read for each alias. Neither walk runs when its
/// own pattern list is empty or once shutdown begins.
async fn warn_forbidden_names(services: &Services) -> Result {
	let progress = &services.server.progress;

	services.server.check_running()?;

	if !services
		.config
		.registration
		.forbidden_usernames
		.is_empty()
	{
		progress.begin("scan_forbidden_usernames");

		forbidden_usernames(services)
			.ready_try_for_each(|Username { user_id, patterns }| {
				warn!(%user_id, %patterns, "User matches forbidden username patterns");
				Ok(())
			})
			.await?;
	}

	services.server.check_running()?;

	if !services
		.config
		.rooms
		.forbidden_alias_names
		.is_empty()
	{
		progress.begin("scan_forbidden_alias_names");

		services
			.metadata
			.iter_ids()
			.map(|room_id| {
				services
					.server
					.check_running()
					.map(|()| room_id.to_owned())
			})
			.try_for_each(async |room_id| scan_room_aliases(services, &room_id).await)
			.await?;
	}

	Ok(())
}

async fn scan_room_aliases(services: &Services, room_id: &RoomId) -> Result {
	let patterns = &services.config.rooms.forbidden_alias_names;

	services
		.alias
		.local_aliases_for_room(room_id)
		.map(|alias| services.server.check_running().map(|()| alias))
		.ready_try_for_each(|alias| {
			if let Some(patterns) = forbidden_patterns(patterns, alias.alias()) {
				warn!(
					%room_id,
					%alias,
					%patterns,
					"Room alias matches the following forbidden alias name patterns"
				);
			}

			Ok(())
		})
		.await
}

/// Whether a named migration step still needs to run, refusing once shutdown
/// begins.
///
/// A step gate is the safe place to observe a stop request: every step that has
/// already run stamped its marker, so the ladder is consistent here and the
/// remaining steps resume on the next start. A step about to run is also named
/// as the phase in flight, so the operator sees which one a long boot is
/// spending its time on.
async fn pending(services: &Services, marker: &'static str) -> Result<bool> {
	services.server.check_running()?;

	let pending = !marker_present(services, marker).await?;

	if pending {
		services.server.progress.begin(marker);
	}

	Ok(pending)
}

/// Whether a migration step has stamped its marker.
///
/// Only a missing marker reads as absent. A read that fails propagates, so a
/// step is never skipped on the strength of a failed read.
pub(crate) async fn marker_present(services: &Services, marker: &str) -> Result<bool> {
	services.db["global"]
		.get(marker)
		.await
		.optional()
		.inspect_err(|error| warn!(%marker, %error, "Migration marker failed to read"))
		.map(|stamp| stamp.is_some())
}

/// Assembles a local user id from a localpart a foreign column records.
///
/// The id is formatted into an inline buffer and parsed from that slice, which
/// keeps a short id in inline storage; parsing against a server name instead
/// routes through an over-allocated `String` and spills to the heap.
pub(crate) fn local_user_id(localpart: &str, server_name: &ServerName) -> Option<OwnedUserId> {
	let user_id: UserIdBuf = format_small_string!("@{localpart}:{server_name}");

	UserId::parse(user_id.as_str()).ok()
}
