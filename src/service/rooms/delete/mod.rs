use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use ruma::{OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, debug, result::LogErr, trace, utils::future::BoolExt, warn};

use crate::rooms::timeline::RoomMutexGuard;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
}

/// Records local-user eviction results and aliases targeted for removal.
///
/// Its serialized layout matches Synapse's `ShutdownRoom`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ShutdownRoom {
	pub kicked_users: Vec<OwnedUserId>,
	pub failed_to_kick_users: Vec<OwnedUserId>,
	pub local_aliases: Vec<OwnedRoomAliasId>,
	pub new_room_id: Option<OwnedRoomId>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self { services: args.services.clone() }))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	pub async fn delete_if_empty_local(&self, room_id: &RoomId, state_lock: RoomMutexGuard) {
		debug_assert!(
			self.services.config.delete_rooms_after_leave,
			"Caller must checking if delete_rooms_after_leave configured."
		);

		let has_local_users = self
			.services
			.state_cache
			.local_users_in_room(room_id)
			.boxed()
			.into_future()
			.map(|(next, ..)| next.as_ref().is_some());

		let has_local_invites = self
			.services
			.state_cache
			.local_users_invited_to_room(room_id)
			.boxed()
			.into_future()
			.map(|(next, ..)| next.as_ref().is_some());

		if has_local_users.or(has_local_invites).await {
			trace!(?room_id, "Not deleting with local joined or invited");
			return;
		}

		debug!(?room_id, "Preparing to delete room...");

		self.services
			.delete
			.delete_room(room_id, false, state_lock)
			.boxed()
			.await
			.expect("unhandled error during room deletion");
	}

	pub async fn delete_room(
		&self,
		room_id: &RoomId,
		force: bool,
		state_lock: RoomMutexGuard,
	) -> Result<ShutdownRoom> {
		let summary = self.shutdown_room(room_id, &state_lock).await;

		self.purge_room(room_id, force, &state_lock).await;

		debug!(?room_id, "Successfully deleted room from our database");

		Ok(summary)
	}

	/// Evicts every local user, strips the room's local aliases, and
	/// unpublishes it from the directory, returning the shutdown summary. The
	/// admin delete runs this phase alone when `purge` is false.
	pub async fn shutdown_room(
		&self,
		room_id: &RoomId,
		state_lock: &RoomMutexGuard,
	) -> ShutdownRoom {
		debug!(?room_id, "Making all local users leave the room and forgetting it");
		let (kicked_users, failed_to_kick_users) = self
			.services
			.state_cache
			.local_users_in_room(room_id)
			.map(ToOwned::to_owned)
			.fold((Vec::new(), Vec::new()), async |(mut kicked, mut failed), user_id| {
				match self
					.services
					.membership
					.leave(&user_id, room_id, Some("Room Deleted".into()), true, state_lock)
					.await
				{
					| Ok(()) => kicked.push(user_id),
					| Err(e) => {
						warn!(%e, "Failed to leave room");
						failed.push(user_id);
					},
				}

				(kicked, failed)
			})
			.await;

		debug!("Deleting all our room aliases for the room");
		let local_aliases = self
			.services
			.alias
			.local_aliases_for_room(room_id)
			.map(ToOwned::to_owned)
			.collect::<Vec<_>>()
			.await;

		for alias in &local_aliases {
			self.services
				.alias
				.remove_alias(alias)
				.await
				.log_err()
				.ok();
		}

		debug!("Removing/unpublishing room from our room directory");
		self.services.directory.set_not_public(room_id);

		ShutdownRoom {
			kicked_users,
			failed_to_kick_users,
			local_aliases,
			new_room_id: None,
		}
	}

	/// Wipes the room's storage. `force` widens the erasure of local users'
	/// left-state (it is not Synapse's `force_purge`).
	async fn purge_room(&self, room_id: &RoomId, force: bool, state_lock: &RoomMutexGuard) {
		debug!("Deleting room's threads from database");
		self.services
			.threads
			.delete_all_rooms_threads(room_id)
			.await
			.log_err()
			.ok();

		debug!("Deleting all the room's search token IDs from our database");
		self.services
			.search
			.delete_all_search_tokenids_for_room(room_id)
			.await
			.log_err()
			.ok();

		debug!("Deleting all room's forward extremities from our database");
		self.services
			.state
			.delete_all_rooms_forward_extremities(room_id)
			.await
			.log_err()
			.ok();

		debug!("Deleting all the room's event (PDU) references");
		self.services
			.pdu_metadata
			.delete_all_referenced_for_room(room_id)
			.await
			.log_err()
			.ok();

		debug!("Deleting all the room's typed relation index entries");
		self.services
			.pdu_metadata
			.delete_all_relatesto_typed_for_room(room_id)
			.await
			.log_err()
			.ok();

		debug!("Deleting all the room's member counts");
		self.services
			.state_cache
			.delete_room_join_counts(room_id, force)
			.await
			.log_err()
			.ok();

		debug!("Deleting all the room's private read receipts");
		self.services
			.read_receipt
			.delete_all_read_receipts(room_id)
			.await
			.log_err()
			.ok();

		debug!("Deleting the room's last notifications read.");
		self.services
			.pusher
			.delete_room_notification_read(room_id)
			.await
			.log_err()
			.ok();

		debug!("Deleting room state hash from our database");
		self.services
			.state
			.delete_room_shortstatehash(room_id, state_lock)
			.log_err()
			.ok();

		debug!("Deleting PDUs");
		self.services
			.timeline
			.delete_pdus(room_id)
			.await
			.log_err()
			.ok();

		debug!("Deleting internal room ID from our database");
		self.services
			.short
			.delete_shortroomid(room_id)
			.await
			.log_err()
			.ok();
	}
}
