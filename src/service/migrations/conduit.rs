use std::{
	collections::BTreeMap,
	iter::from_fn,
	path::{Path, PathBuf},
	pin::pin,
	sync::Arc,
	time::Duration,
};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use object_store::Error as ObjectStoreError;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, Mxc, OwnedRoomId, OwnedUserId, RoomId,
	ServerName, UserId,
};
use serde::{Deserialize, de::IgnoredAny};
use tokio::time::sleep;
use tuwunel_core::{
	Err, Error, Result, debug_warn, err, error, info,
	itertools::Itertools,
	utils,
	utils::{ReadyExt, content_disposition::make_content_disposition, stream::TryIgnore},
	warn,
};
use tuwunel_database::{Map, SEP};

use crate::{Services, storage::Provider};

/// Presence probe: `room_id` parses to `Some` when the stored PDU carries it.
#[derive(Deserialize)]
struct HasRoomId {
	room_id: Option<IgnoredAny>,
}

/// Where a Conduit database kept its original media files: on the local
/// filesystem (the default), or in an object store named by
/// `conduit_source_media_provider` for a Conduit that backed its media with S3.
enum MediaSource {
	Filesystem(PathBuf),
	Provider(Arc<Provider>),
}

/// One parsed `servernamemediaid_metadata` entry, borrowing the raw key/value.
struct ConduitMediaEntry<'a> {
	server_name: &'a ServerName,
	media_id: &'a str,
	sha256: &'a [u8],
	filename: Option<&'a str>,
	content_type: Option<&'a str>,
}

/// Resolves the configured Conduit media source. A named provider must already
/// be defined under `[storage_provider]`; its absence is an operator error that
/// aborts the import rather than silently dropping every file.
fn media_source(services: &Services) -> Result<MediaSource> {
	let config = &services.server.config;

	match config.conduit_source_media_provider.as_deref() {
		| Some(name) => services
			.storage
			.provider(name)
			.map(|provider| MediaSource::Provider(provider.clone())),
		| None => {
			let media_dir = config
				.conduit_source_media_path
				.clone()
				.unwrap_or_else(|| config.database_path.join("media"));

			Ok(MediaSource::Filesystem(media_dir))
		},
	}
}

/// Attempts to read each original from a source storage provider before the
/// import gives up: one initial try plus one retry. The object store performs
/// its own internal retries under each attempt, so a transient provider blip
/// rarely reaches this outer limit.
const PROVIDER_READ_ATTEMPTS: u32 = 2;

/// Pause between provider read retries, giving a transient fault time to clear.
const PROVIDER_READ_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Imports the original media files of a Conduit database, re-uploading each
/// `servernamemediaid_metadata` entry through `media.create`.
///
/// Malformed entries, unreadable filesystem files, missing provider objects,
/// and moderator-blocked media are skipped. Persistent source-provider faults,
/// blocklist read errors, and destination creation failures abort without
/// setting the completion marker; a restart safely overwrites deterministic
/// partial metadata and provider objects.
pub(super) async fn migrate_conduit_media(services: &Services) -> Result {
	let db = &services.db;
	let config = &services.server.config;

	let Some(metadata) = db.open_cf("servernamemediaid_metadata")? else {
		warn!("Conduit database has no media metadata; nothing to import.");
		return Ok(());
	};

	let owners = db.open_cf("servernamemediaid_userlocalpart")?;
	let owners = owners.as_ref();

	let blocklist = db.open_cf("blocked_servername_mediaid")?;
	let blocklist = blocklist.as_ref();

	let depth = config.conduit_media_directory_depth;
	let length = config.conduit_media_directory_length;
	let source = media_source(services)?;

	warn!("Importing Conduit media originals into tuwunel's key-addressed store...");

	let cork = db.cork_and_sync();
	let (imported, skipped, blocked) = metadata
		.raw_stream()
		.ignore_err()
		.map(Ok::<_, Error>)
		.try_fold(
			(0_usize, 0_usize, 0_usize),
			async |(imported, skipped, blocked), (key, value)| {
				if conduit_media_blocked(blocklist, key).await? {
					return Ok((imported, skipped, blocked.saturating_add(1)));
				}

				let imported_entry =
					import_conduit_original(services, owners, &source, depth, length, key, value)
						.await?;

				Ok(if imported_entry {
					(imported.saturating_add(1), skipped, blocked)
				} else {
					(imported, skipped.saturating_add(1), blocked)
				})
			},
		)
		.await?;

	drop(cork);

	if blocked > 0 {
		warn!(%blocked, "Skipped Conduit media blocked by a moderator; not imported");
	}

	if skipped > 0 {
		warn!(%imported, %skipped, "Imported Conduit media originals; some files were skipped");
	} else {
		info!(%imported, "Imported Conduit media originals");
	}

	Ok(())
}

/// Imports one `servernamemediaid_metadata` entry and reports whether it was
/// stored.
///
/// `false` denotes an intentional source-entry skip. Destination creation
/// errors propagate so the caller can withhold the completion marker.
async fn import_conduit_original(
	services: &Services,
	owners: Option<&Arc<Map>>,
	source: &MediaSource,
	depth: u8,
	length: u8,
	key: &[u8],
	value: &[u8],
) -> Result<bool> {
	let entry = match parse_conduit_media_entry(key, value) {
		| Ok(entry) => entry,
		| Err(e) => {
			debug_warn!(error = %e, "skipping unimportable Conduit media entry");
			return Ok(false);
		},
	};

	let Some(file) = read_conduit_original(source, depth, length, entry.sha256).await? else {
		return Ok(false);
	};

	let content_disposition = make_content_disposition(None, entry.content_type, entry.filename);
	let owner = conduit_media_owner(owners, key, entry.server_name).await;
	let mxc = Mxc {
		server_name: entry.server_name,
		media_id: entry.media_id,
	};

	services
		.media
		.create(&mxc, owner.as_deref(), Some(&content_disposition), entry.content_type, &file)
		.await?;

	Ok(true)
}

/// Parses a `servernamemediaid_metadata` entry: the key is
/// `servername 0xff media_id`, the value is the digest, filename and
/// content type.
fn parse_conduit_media_entry<'a>(
	key: &'a [u8],
	value: &'a [u8],
) -> Result<ConduitMediaEntry<'a>> {
	let Some(sep) = key.iter().position(|&byte| byte == SEP) else {
		return Err!(Database("Conduit media key has no server-name separator"));
	};
	let server_name = <&ServerName>::try_from(str::from_utf8(&key[..sep])?)
		.map_err(|_| err!(Database("Conduit media key has an invalid server name")))?;

	let media_id = str::from_utf8(&key[sep.saturating_add(1)..])?;

	let (sha256, filename, content_type) = parse_conduit_media_value(value)?;

	Ok(ConduitMediaEntry {
		server_name,
		media_id,
		sha256,
		filename,
		content_type,
	})
}

/// Reads one Conduit original, named by its content digest, from the configured
/// media source. A filesystem file that cannot be read is reported as `None`
/// (skipped, like a dangling metadata row). A source storage provider is
/// retried on a transient fault; a persistent one returns `Err` so the import
/// aborts instead of dropping reachable media.
async fn read_conduit_original(
	source: &MediaSource,
	depth: u8,
	length: u8,
	sha256: &[u8],
) -> Result<Option<Bytes>> {
	let sha256_hex = sha256_hex(sha256);
	match source {
		| MediaSource::Filesystem(media_dir) => {
			let path = conduit_media_path(media_dir, depth, length, &sha256_hex);
			match tokio::fs::read(&path).await {
				| Ok(file) => Ok(Some(file.into())),
				| Err(e) => {
					debug_warn!(?path, error = %e, "skipping unreadable Conduit media file");
					Ok(None)
				},
			}
		},
		| MediaSource::Provider(provider) =>
			read_provider_original(provider, &conduit_media_key(depth, length, &sha256_hex)).await,
	}
}

/// Reads one original from the source storage provider. An absent object is
/// skipped (`Ok(None)`) like a dangling filesystem row; a transient fault is
/// retried up to `PROVIDER_READ_ATTEMPTS` times, and a persistent one aborts
/// the import with an `Err`.
async fn read_provider_original(provider: &Arc<Provider>, key: &str) -> Result<Option<Bytes>> {
	let mut attempt = 0_u32;
	loop {
		attempt = attempt.saturating_add(1);
		match provider.get(key).await {
			| Ok(file) => return Ok(Some(file)),
			| Err(e) if is_missing_object(&e) => {
				debug_warn!(%key, error = %e, "skipping missing Conduit media object");
				return Ok(None);
			},
			| Err(e) if attempt >= PROVIDER_READ_ATTEMPTS => {
				error!(
					%key,
					attempts = PROVIDER_READ_ATTEMPTS,
					error = %e,
					"Aborting the Conduit media import: source storage provider unreachable. No \
					 media has been imported in a way that needs cleanup; once the provider is \
					 reachable, restart tuwunel to resume the import from the beginning."
				);
				return Err(e);
			},
			| Err(e) => {
				warn!(%key, attempt, error = %e, "Reading Conduit media object failed; retrying");
				sleep(PROVIDER_READ_RETRY_DELAY).await;
			},
		}
	}
}

/// Whether a provider read failed because the object is absent (a 404 or a
/// dangling metadata row), which is skipped rather than retried. tuwunel's
/// `Error::is_not_found` does not cover the object-store variant, so match it
/// directly.
fn is_missing_object(error: &Error) -> bool {
	matches!(error, Error::ObjectStore(ObjectStoreError::NotFound { .. }))
}

/// Splits a `servernamemediaid_metadata` value into its digest, filename, and
/// content type. The value is `sha256(32) | filename | 0xff | content_type`
/// with an optional trailing `0xff` that Conduit's media-auth migration appends
/// to flag unauthenticated access; that flag is ignored.
fn parse_conduit_media_value(value: &[u8]) -> Result<(&[u8], Option<&str>, Option<&str>)> {
	let (sha256, rest) = value
		.split_at_checked(32)
		.ok_or_else(|| err!(Database("Conduit media value shorter than a SHA-256 digest")))?;

	// Take filename and content_type, ignoring the optional trailing 0xff flag.
	let mut parts = rest.split(|&byte| byte == SEP);
	let filename = parts.next().unwrap_or_default();
	let Some(content_type) = parts.next() else {
		return Err!(Database("Conduit media value has no content-type separator"));
	};
	let filename = str::from_utf8(filename)?;
	let content_type = str::from_utf8(content_type)?;
	let filename = (!filename.is_empty()).then_some(filename);
	let content_type = (!content_type.is_empty()).then_some(content_type);

	Ok((sha256, filename, content_type))
}

/// The local owner of a Conduit media entry from
/// `servernamemediaid_userlocalpart`; `None` for remote media, which has no
/// such entry.
async fn conduit_media_owner(
	owners: Option<&Arc<Map>>,
	key: &[u8],
	server_name: &ServerName,
) -> Option<OwnedUserId> {
	let localpart = owners?.get(key).await.ok()?;

	UserId::parse_with_server_name(str::from_utf8(&localpart).ok()?, server_name).ok()
}

/// Whether a Conduit media entry was blocked by a moderator. Conduit keeps the
/// file and refuses it only at read time (`blocked_servername_mediaid`);
/// tuwunel has no per-media blocklist, so importing a blocked original would
/// serve it again. The blocklist key is `server_name 0xff media_id`, the same
/// bytes as the `servernamemediaid_metadata` key, so the entry's raw key probes
/// it directly. Only a clean miss imports; a hard read error aborts the import
/// (like an unreachable source) rather than silently re-serving blocked media.
async fn conduit_media_blocked(blocklist: Option<&Arc<Map>>, key: &[u8]) -> Result<bool> {
	let Some(blocklist) = blocklist else {
		return Ok(false);
	};

	match blocklist.exists(key).await {
		| Ok(()) => Ok(true),
		| Err(e) if e.is_not_found() => Ok(false),
		| Err(e) => Err(e),
	}
}

/// Reconstructs the on-disk path of a Conduit content-addressed media file from
/// the lowercase SHA-256 hex digest naming it, matching Conduit's
/// `split_media_path`: `media_dir` joined with the digest's shard segments.
fn conduit_media_path(media_dir: &Path, depth: u8, length: u8, sha256_hex: &str) -> PathBuf {
	let mut path = media_dir.to_path_buf();
	path.extend(conduit_shards(depth, length, sha256_hex));
	path
}

/// The object-store key of a Conduit content-addressed media object, the same
/// shard segments as `conduit_media_path` joined by `/`. The source provider's
/// `base_path` supplies any bucket prefix (Conduit's `media.path`).
fn conduit_media_key(depth: u8, length: u8, sha256_hex: &str) -> String {
	conduit_shards(depth, length, sha256_hex).join("/")
}

/// Splits a lowercase SHA-256 hex digest into Conduit's shard segments: `depth`
/// segments of `length` characters then the remainder, or the whole digest when
/// `depth` is zero (a flat layout). `config::check` bounds `depth * length`
/// below the digest length so the segments never overrun it.
fn conduit_shards(depth: u8, length: u8, sha256_hex: &str) -> impl Iterator<Item = &str> {
	let mut rest = Some(sha256_hex);
	let mut remaining = depth;
	from_fn(move || {
		let current = rest?;
		if remaining == 0 {
			rest = None;
			return Some(current);
		}

		remaining = remaining.saturating_sub(1);
		match current.split_at_checked(length.into()) {
			| Some((segment, next)) => {
				rest = Some(next);
				Some(segment)
			},
			| None => {
				rest = None;
				Some(current)
			},
		}
	})
}

/// Lowercase hex digest, matching the names Conduit gives its media files.
fn sha256_hex(digest: &[u8]) -> String {
	const HEX: &[u8; 16] = b"0123456789abcdef";

	let mut out = String::with_capacity(digest.len().saturating_mul(2));
	for &byte in digest {
		out.push(char::from(HEX[usize::from(byte >> 4)]));
		out.push(char::from(HEX[usize::from(byte & 0x0F)]));
	}

	out
}

/// Injects `room_id` into stored PDUs that lack it. Runs once on every database
/// (marker-gated by the caller); a native tuwunel DB always serializes the
/// field, so it no-ops there. Only a room v12 (`hydra`) create event imported
/// from Conduit omits it, deriving its room from the event's own id per
/// MSC4291. Scans the `pduid_pdu` timeline (room from the key's leading short
/// room id) and `eventid_outlierpdu` (room from the create event's own id, the
/// outlier key).
pub(super) async fn migrate_conduit_pdus(services: &Services) -> Result {
	let db = &services.db;
	let progress = &services.server.progress;

	// shortroomid -> room_id, inverted once so resolving each timeline PDU's
	// room is a lookup rather than a scan of roomid_shortroomid.
	let rooms: BTreeMap<u64, OwnedRoomId> = db["roomid_shortroomid"]
		.stream()
		.ignore_err()
		.map(|(room_id, short): (&RoomId, u64)| (short, room_id.to_owned()))
		.collect()
		.await;

	warn!("Ensuring stored PDUs carry their room_id field...");
	let cork = db.cork_and_sync();

	let pduid_pdu = &db["pduid_pdu"];

	progress.enter("timeline rows");
	let timeline = pduid_pdu
		.raw_stream()
		.ignore_err()
		.ready_fold((0_usize, 0_usize), |acc, (key, value)| {
			progress.advance();

			tally(acc, inject_room_id(pduid_pdu, key, value, |_| pduid_room(&rooms, key)))
		})
		.await;

	let outlier = &db["eventid_outlierpdu"];

	progress.enter("outlier rows");
	let outliers = outlier
		.raw_stream()
		.ignore_err()
		.ready_fold((0_usize, 0_usize), |acc, (key, value)| {
			progress.advance();

			tally(acc, inject_room_id(outlier, key, value, |pdu| outlier_room(key, pdu)))
		})
		.await;

	drop(cork);

	let fixed = timeline.0.saturating_add(outliers.0);
	let skipped = timeline.1.saturating_add(outliers.1);
	if skipped > 0 {
		warn!(%fixed, %skipped, "Injected room_id into stored PDUs; some were skipped");
	} else {
		info!(%fixed, "Ensured stored PDUs carry room_id");
	}

	Ok(())
}

fn tally((fixed, skipped): (usize, usize), result: Result<bool>) -> (usize, usize) {
	match result {
		| Ok(true) => (fixed.saturating_add(1), skipped),
		| Ok(false) => (fixed, skipped),
		| Err(e) => {
			debug_warn!(error = %e, "skipping unreconcilable Conduit PDU");
			(fixed, skipped.saturating_add(1))
		},
	}
}

/// Injects `room_id` into one PDU value that lacks it, sourcing the room from
/// `resolve`. Returns whether the value was rewritten; `false` means it already
/// carried a `room_id`. A cheap `HasRoomId` probe short-circuits that common
/// case, so only the rewritten PDUs pay the full parse and re-serialize.
fn inject_room_id(
	map: &Arc<Map>,
	key: &[u8],
	value: &[u8],
	resolve: impl FnOnce(&CanonicalJsonObject) -> Result<OwnedRoomId>,
) -> Result<bool> {
	let probe: HasRoomId = serde_json::from_slice(value)
		.map_err(|e| err!(Database("Conduit PDU is not canonical JSON: {e}")))?;

	if probe.room_id.is_some() {
		return Ok(false);
	}

	let mut pdu: CanonicalJsonObject = serde_json::from_slice(value)
		.map_err(|e| err!(Database("Conduit PDU is not canonical JSON: {e}")))?;

	let room_id = resolve(&pdu)?;
	pdu.insert("room_id".into(), CanonicalJsonValue::String(room_id.as_str().into()));

	let bytes = serde_json::to_vec(&pdu)
		.map_err(|e| err!(Database("re-serializing reconciled Conduit PDU: {e}")))?;

	map.insert(key, bytes);

	Ok(true)
}

/// The room of a `pduid_pdu` entry, from the short room id leading its key.
fn pduid_room(rooms: &BTreeMap<u64, OwnedRoomId>, key: &[u8]) -> Result<OwnedRoomId> {
	let short = key
		.get(..8)
		.ok_or_else(|| err!(Database("Conduit pduid is shorter than a short room id")))?;

	rooms
		.get(&utils::u64_from_u8(short))
		.cloned()
		.ok_or_else(|| err!(Database("Conduit pduid short room id maps to no room")))
}

/// The room of an `eventid_outlierpdu` entry that lacks `room_id`. Only a v12
/// create event omits it, and its room id derives from the create event's own
/// id, which is this outlier's key.
fn outlier_room(key: &[u8], pdu: &CanonicalJsonObject) -> Result<OwnedRoomId> {
	let is_create = matches!(
		pdu.get("type"),
		Some(CanonicalJsonValue::String(kind)) if kind == "m.room.create"
	);

	if !is_create {
		return Err!(Database("Conduit outlier lacks room_id and is not a create event"));
	}

	let event_id = <&EventId>::try_from(str::from_utf8(key)?)
		.map_err(|_| err!(Database("Conduit outlier key is not a valid event id")))?;

	RoomId::new_v2(event_id.localpart())
		.map_err(|e| err!(Database("deriving room id from create event id: {e}")))
}

/// Imports Conduit's pending knocks. Conduit names the columns
/// `roomuserid_knockcount` / `userroomid_knockstate`; tuwunel renamed them to
/// `*knocked*` but kept the byte layout (`room_id 0xff user_id` -> u64 count;
/// `user_id 0xff room_id` -> JSON stripped state), so each row copies verbatim.
/// Imported once: tuwunel clears a knock on the user's join or leave, so a
/// re-import would resurrect a knock the user has already resolved.
pub(super) async fn migrate_conduit_knocks(services: &Services) -> Result {
	let knocks = copy_cf(services, "roomuserid_knockcount", "roomuserid_knockedcount").await?;
	copy_cf(services, "userroomid_knockstate", "userroomid_knockedstate").await?;

	if knocks > 0 {
		warn!(%knocks, "Imported Conduit knocks");
	}

	Ok(())
}

/// Splits Conduit's conflated highlight-count column. Conduit opens
/// `roomuserid_lastnotificationread` against the `userroomid_highlightcount`
/// tree (a copy-paste in its schema), so one column holds both stores:
/// highlight counts keyed `user_id 0xff room_id` and last-notification-read
/// tokens keyed `room_id 0xff user_id`. tuwunel keeps the two in separate
/// columns with those same byte layouts, so every room-keyed (last-read) row
/// moves verbatim into `roomuserid_lastnotificationread`, leaving the
/// user-keyed highlight rows in place. The orderings never collide: a user id
/// leads with `@`, a room id with `!`. Absent any room-keyed row the column is
/// not aliased, so this returns early and is safe to run on a native database.
pub(super) async fn migrate_conduit_highlight_split(services: &Services) -> Result {
	let db = &services.db;
	let highlight = db["userroomid_highlightcount"].clone();

	// A room-keyed (last-read) row leads with '!'; without one the column is a
	// plain highlight column needing no split.
	if pin!(highlight.raw_keys_prefix(b"!"))
		.next()
		.await
		.is_none()
	{
		return Ok(());
	}

	let lastread = db["roomuserid_lastnotificationread"].clone();
	let cork = db.cork_and_sync();
	let moved = highlight
		.raw_stream()
		.ignore_err()
		.ready_fold(0_usize, |moved, (key, value)| {
			if key.first() == Some(&b'!') {
				lastread.insert(key, value);
				highlight.remove(key);
				moved.saturating_add(1)
			} else {
				moved
			}
		})
		.await;

	drop(cork);

	if moved > 0 {
		warn!(%moved, "Split Conduit last-notification-read rows out of the highlight-count column");
	}

	Ok(())
}

/// Copies every row of one column verbatim into another whose key and value
/// share the same byte layout, so neither needs reserialization.
async fn copy_cf(
	services: &Services,
	source_name: &'static str,
	target_name: &'static str,
) -> Result<usize> {
	let db = &services.db;
	let Some(source) = db.open_cf(source_name)? else {
		return Ok(0);
	};

	let target = &db[target_name];
	let cork = db.cork_and_sync();
	let copied = source
		.raw_stream()
		.ignore_err()
		.ready_fold(0_usize, |copied, (key, value)| {
			target.insert(key, value);
			copied.saturating_add(1)
		})
		.await;

	drop(cork);

	Ok(copied)
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{
		HasRoomId, conduit_media_key, conduit_media_path, parse_conduit_media_value, sha256_hex,
	};

	#[test]
	fn conduit_media_path_deep_matches_conduit_default() {
		// Conduit default Deep { length: 2, depth: 2 }: two 2-char segments of the
		// 64-char digest, then the remaining 60 characters.
		let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
		let path = conduit_media_path(Path::new("/db/media"), 2, 2, hex);

		assert_eq!(
			path,
			Path::new(
				"/db/media/01/23/456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
			)
		);
	}

	#[test]
	fn conduit_media_path_flat_is_unsharded() {
		let path = conduit_media_path(Path::new("/db/media"), 0, 2, "abcdef");

		assert_eq!(path, Path::new("/db/media/abcdef"));
	}

	#[test]
	fn conduit_media_key_deep_joins_shards_with_slash() {
		// The object key carries no media_dir; the source provider's base_path
		// supplies any prefix. Same shard segments as the Deep on-disk path.
		let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
		let key = conduit_media_key(2, 2, hex);

		assert_eq!(key, "01/23/456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
	}

	#[test]
	fn conduit_media_key_flat_is_bare_digest() {
		assert_eq!(conduit_media_key(0, 2, "abcdef"), "abcdef");
	}

	#[test]
	fn sha256_hex_encodes_lowercase_padded() {
		assert_eq!(sha256_hex(&[0x00, 0x0F, 0xFF, 0xA5]), "000fffa5");
	}

	#[test]
	fn conduit_media_value_ignores_unauthenticated_flag() {
		// Conduit's media-auth migration appends a trailing 0xff after content_type.
		let mut value = vec![7_u8; 32];
		value.extend_from_slice(b"pic.png");
		value.push(0xFF);
		value.extend_from_slice(b"image/png");
		value.push(0xFF);

		let (sha256, filename, content_type) = parse_conduit_media_value(&value).unwrap();

		assert_eq!(sha256, [7_u8; 32].as_slice());
		assert_eq!(filename, Some("pic.png"));
		assert_eq!(content_type, Some("image/png"));
	}

	#[test]
	fn conduit_media_value_empty_filename_is_none() {
		let mut value = vec![0_u8; 32];
		value.push(0xFF);
		value.extend_from_slice(b"image/png");

		let (_, filename, content_type) = parse_conduit_media_value(&value).unwrap();

		assert_eq!(filename, None);
		assert_eq!(content_type, Some("image/png"));
	}

	#[test]
	fn has_room_id_probe_detects_presence() {
		let with_room_id = br#"{"room_id":"!r:server","type":"m.room.message"}"#;
		let without_room_id = br#"{"type":"m.room.create","sender":"@u:server"}"#;

		let present: HasRoomId = serde_json::from_slice(with_room_id).unwrap();
		let absent: HasRoomId = serde_json::from_slice(without_room_id).unwrap();

		assert!(present.room_id.is_some());
		assert!(absent.room_id.is_none());
	}
}
