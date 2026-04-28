use std::cmp::Ordering;

use axum::extract::State;
use futures::{FutureExt, future::try_join};
use ruma::{
	UInt, UserId,
	api::client::backup::{
		add_backup_keys, add_backup_keys_for_room, add_backup_keys_for_session,
		create_backup_version, delete_backup_keys, delete_backup_keys_for_room,
		delete_backup_keys_for_session, delete_backup_version, get_backup_info, get_backup_keys,
		get_backup_keys_for_room, get_backup_keys_for_session, get_latest_backup_info,
		update_backup_version,
	},
	serde::Raw,
};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Err, Result, err};
use tuwunel_service::Services;

use crate::Ruma;

/// Overrides ruma's internal `AlgorithmWithData` shape required by the GET
/// `/room_keys/version[/{version}]` response serializer. Validating against
/// this will not raise a serialization error (HTTP 500) when responding.
#[derive(Deserialize)]
#[expect(unused)]
struct AlgorithmShape {
	algorithm: Box<RawJsonValue>,
	auth_data: Box<RawJsonValue>,
}

fn validate_algorithm_shape<T>(raw: &Raw<T>) -> Result {
	raw.deserialize_as_unchecked::<AlgorithmShape>()
		.map_err(Into::into)
		.map(drop)
}

/// # `POST /_matrix/client/r0/room_keys/version`
///
/// Creates a new backup.
pub(crate) async fn create_backup_version_route(
	State(services): State<crate::State>,
	body: Ruma<create_backup_version::v3::Request>,
) -> Result<create_backup_version::v3::Response> {
	validate_algorithm_shape(&body.algorithm)
		.map_err(|e| err!(Request(BadJson("Invalid backup metadata: {e}"))))?;

	let version = services
		.key_backups
		.create_backup(body.sender_user(), &body.algorithm)?;

	Ok(create_backup_version::v3::Response { version })
}

/// # `PUT /_matrix/client/r0/room_keys/version/{version}`
///
/// Update information about an existing backup. Only `auth_data` can be
/// modified.
pub(crate) async fn update_backup_version_route(
	State(services): State<crate::State>,
	body: Ruma<update_backup_version::v3::Request>,
) -> Result<update_backup_version::v3::Response> {
	validate_algorithm_shape(&body.algorithm)
		.map_err(|e| err!(Request(BadJson("Invalid backup metadata: {e}"))))?;

	services
		.key_backups
		.update_backup(body.sender_user(), &body.version, &body.algorithm)
		.await?;

	Ok(update_backup_version::v3::Response {})
}

/// # `GET /_matrix/client/r0/room_keys/version`
///
/// Get information about the latest backup version.
pub(crate) async fn get_latest_backup_info_route(
	State(services): State<crate::State>,
	body: Ruma<get_latest_backup_info::v3::Request>,
) -> Result<get_latest_backup_info::v3::Response> {
	let (version, algorithm) = services
		.key_backups
		.get_latest_backup(body.sender_user())
		.await
		.map_err(|_| err!(Request(NotFound("Key backup does not exist."))))?;

	validate_algorithm_shape(&algorithm)
		.map_err(|e| err!(Request(NotFound("Key backup does not exist: {e}"))))?;

	let (count, etag) = get_count_etag(&services, body.sender_user(), &version).await?;

	Ok(get_latest_backup_info::v3::Response { algorithm, count, etag, version })
}

/// # `GET /_matrix/client/v3/room_keys/version/{version}`
///
/// Get information about an existing backup.
pub(crate) async fn get_backup_info_route(
	State(services): State<crate::State>,
	body: Ruma<get_backup_info::v3::Request>,
) -> Result<get_backup_info::v3::Response> {
	let algorithm = services
		.key_backups
		.get_backup(body.sender_user(), &body.version)
		.await
		.map_err(|_| {
			err!(Request(NotFound("Key backup does not exist at version {:?}", body.version)))
		})?;

	validate_algorithm_shape(&algorithm).map_err(|e| {
		err!(Request(NotFound(
			"Key backup does not exist at version {:?}: {e}",
			body.version
		)))
	})?;

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(get_backup_info::v3::Response {
		algorithm,
		count,
		etag,
		version: body.version.clone(),
	})
}

/// # `DELETE /_matrix/client/r0/room_keys/version/{version}`
///
/// Delete an existing key backup.
///
/// - Deletes both information about the backup, as well as all key data related
///   to the backup
pub(crate) async fn delete_backup_version_route(
	State(services): State<crate::State>,
	body: Ruma<delete_backup_version::v3::Request>,
) -> Result<delete_backup_version::v3::Response> {
	services
		.key_backups
		.delete_backup(body.sender_user(), &body.version)
		.await;

	Ok(delete_backup_version::v3::Response {})
}

/// # `PUT /_matrix/client/r0/room_keys/keys`
///
/// Add the received backup keys to the database.
///
/// - Only manipulating the most recently created version of the backup is
///   allowed
/// - Adds the keys to the backup
/// - Returns the new number of keys in this backup and the etag
pub(crate) async fn add_backup_keys_route(
	State(services): State<crate::State>,
	body: Ruma<add_backup_keys::v3::Request>,
) -> Result<add_backup_keys::v3::Response> {
	if services
		.key_backups
		.get_latest_backup_version(body.sender_user())
		.await
		.is_ok_and(|version| version != body.version)
	{
		return Err!(Request(InvalidParam(
			"You may only manipulate the most recently created version of the backup."
		)));
	}

	for (room_id, room) in &body.rooms {
		for (session_id, key_data) in &room.sessions {
			services
				.key_backups
				.add_key(body.sender_user(), &body.version, room_id, session_id, key_data)
				.await?;
		}
	}

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(add_backup_keys::v3::Response { count, etag })
}

/// # `PUT /_matrix/client/r0/room_keys/keys/{roomId}`
///
/// Add the received backup keys to the database.
///
/// - Only manipulating the most recently created version of the backup is
///   allowed
/// - Adds the keys to the backup
/// - Returns the new number of keys in this backup and the etag
pub(crate) async fn add_backup_keys_for_room_route(
	State(services): State<crate::State>,
	body: Ruma<add_backup_keys_for_room::v3::Request>,
) -> Result<add_backup_keys_for_room::v3::Response> {
	if services
		.key_backups
		.get_latest_backup_version(body.sender_user())
		.await
		.is_ok_and(|version| version != body.version)
	{
		return Err!(Request(InvalidParam(
			"You may only manipulate the most recently created version of the backup."
		)));
	}

	for (session_id, key_data) in &body.sessions {
		services
			.key_backups
			.add_key(body.sender_user(), &body.version, &body.room_id, session_id, key_data)
			.await?;
	}

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(add_backup_keys_for_room::v3::Response { count, etag })
}

/// # `PUT /_matrix/client/r0/room_keys/keys/{roomId}/{sessionId}`
///
/// Add the received backup key to the database.
///
/// - Only manipulating the most recently created version of the backup is
///   allowed
/// - Adds the keys to the backup
/// - Returns the new number of keys in this backup and the etag
pub(crate) async fn add_backup_keys_for_session_route(
	State(services): State<crate::State>,
	body: Ruma<add_backup_keys_for_session::v3::Request>,
) -> Result<add_backup_keys_for_session::v3::Response> {
	if services
		.key_backups
		.get_latest_backup_version(body.sender_user())
		.await
		.is_ok_and(|version| version != body.version)
	{
		return Err!(Request(InvalidParam(
			"You may only manipulate the most recently created version of the backup."
		)));
	}

	// Check if we already have a better key
	let mut ok_to_replace = true;
	if let Some(old_key) = &services
		.key_backups
		.get_session(body.sender_user(), &body.version, &body.room_id, &body.session_id)
		.await
		.ok()
	{
		let old_is_verified = old_key
			.get_field::<bool>("is_verified")?
			.unwrap_or_default();

		let new_is_verified = body
			.session_data
			.get_field::<bool>("is_verified")?
			.ok_or_else(|| err!(Request(BadJson("`is_verified` field should exist"))))?;

		// Prefer key that `is_verified`
		if old_is_verified != new_is_verified {
			if old_is_verified {
				ok_to_replace = false;
			}
		} else {
			// If both have same `is_verified`, prefer the one with lower
			// `first_message_index`
			let old_first_message_index = old_key
				.get_field::<UInt>("first_message_index")?
				.unwrap_or(UInt::MAX);

			let new_first_message_index = body
				.session_data
				.get_field::<UInt>("first_message_index")?
				.ok_or_else(|| {
					err!(Request(BadJson("`first_message_index` field should exist")))
				})?;

			ok_to_replace = match new_first_message_index.cmp(&old_first_message_index) {
				| Ordering::Less => true,
				| Ordering::Greater => false,
				| Ordering::Equal => {
					// If both have same `first_message_index`, prefer the one with lower
					// `forwarded_count`
					let old_forwarded_count = old_key
						.get_field::<UInt>("forwarded_count")?
						.unwrap_or(UInt::MAX);

					let new_forwarded_count = body
						.session_data
						.get_field::<UInt>("forwarded_count")?
						.ok_or_else(|| {
							err!(Request(BadJson("`forwarded_count` field should exist")))
						})?;

					new_forwarded_count < old_forwarded_count
				},
			};
		}
	}

	if ok_to_replace {
		services
			.key_backups
			.add_key(
				body.sender_user(),
				&body.version,
				&body.room_id,
				&body.session_id,
				&body.session_data,
			)
			.await?;
	}

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(add_backup_keys_for_session::v3::Response { count, etag })
}

/// # `GET /_matrix/client/r0/room_keys/keys`
///
/// Retrieves all keys from the backup.
pub(crate) async fn get_backup_keys_route(
	State(services): State<crate::State>,
	body: Ruma<get_backup_keys::v3::Request>,
) -> Result<get_backup_keys::v3::Response> {
	let rooms = services
		.key_backups
		.get_all(body.sender_user(), &body.version)
		.await;

	Ok(get_backup_keys::v3::Response { rooms })
}

/// # `GET /_matrix/client/r0/room_keys/keys/{roomId}`
///
/// Retrieves all keys from the backup for a given room.
pub(crate) async fn get_backup_keys_for_room_route(
	State(services): State<crate::State>,
	body: Ruma<get_backup_keys_for_room::v3::Request>,
) -> Result<get_backup_keys_for_room::v3::Response> {
	let sessions = services
		.key_backups
		.get_room(body.sender_user(), &body.version, &body.room_id)
		.await;

	Ok(get_backup_keys_for_room::v3::Response { sessions })
}

/// # `GET /_matrix/client/r0/room_keys/keys/{roomId}/{sessionId}`
///
/// Retrieves a key from the backup.
pub(crate) async fn get_backup_keys_for_session_route(
	State(services): State<crate::State>,
	body: Ruma<get_backup_keys_for_session::v3::Request>,
) -> Result<get_backup_keys_for_session::v3::Response> {
	let key_data = services
		.key_backups
		.get_session(body.sender_user(), &body.version, &body.room_id, &body.session_id)
		.await
		.map_err(|_| {
			err!(Request(NotFound(debug_error!("Backup key not found for this user's session."))))
		})?;

	Ok(get_backup_keys_for_session::v3::Response { key_data })
}

/// # `DELETE /_matrix/client/r0/room_keys/keys`
///
/// Delete the keys from the backup.
pub(crate) async fn delete_backup_keys_route(
	State(services): State<crate::State>,
	body: Ruma<delete_backup_keys::v3::Request>,
) -> Result<delete_backup_keys::v3::Response> {
	services
		.key_backups
		.delete_all_keys(body.sender_user(), &body.version)
		.await;

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(delete_backup_keys::v3::Response { count, etag })
}

/// # `DELETE /_matrix/client/r0/room_keys/keys/{roomId}`
///
/// Delete the keys from the backup for a given room.
pub(crate) async fn delete_backup_keys_for_room_route(
	State(services): State<crate::State>,
	body: Ruma<delete_backup_keys_for_room::v3::Request>,
) -> Result<delete_backup_keys_for_room::v3::Response> {
	services
		.key_backups
		.delete_room_keys(body.sender_user(), &body.version, &body.room_id)
		.await;

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(delete_backup_keys_for_room::v3::Response { count, etag })
}

/// # `DELETE /_matrix/client/r0/room_keys/keys/{roomId}/{sessionId}`
///
/// Delete a key from the backup.
pub(crate) async fn delete_backup_keys_for_session_route(
	State(services): State<crate::State>,
	body: Ruma<delete_backup_keys_for_session::v3::Request>,
) -> Result<delete_backup_keys_for_session::v3::Response> {
	services
		.key_backups
		.delete_room_key(body.sender_user(), &body.version, &body.room_id, &body.session_id)
		.await;

	let (count, etag) = get_count_etag(&services, body.sender_user(), &body.version).await?;

	Ok(delete_backup_keys_for_session::v3::Response { count, etag })
}

async fn get_count_etag(
	services: &Services,
	sender_user: &UserId,
	version: &str,
) -> Result<(UInt, String)> {
	let count = services
		.key_backups
		.count_keys(sender_user, version)
		.map(TryInto::try_into);

	let etag = services
		.key_backups
		.get_etag(sender_user, version)
		.map(Ok);

	Ok(try_join(count, etag).await?)
}
