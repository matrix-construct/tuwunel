use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use ruma::RoomId;
use tuwunel_core::{
	Result, debug,
	result::LogErr,
	trace,
	utils::{ReadyExt, future::BoolExt},
	warn,
};

use crate::rooms::timeline::RoomMutexGuard;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
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
	) -> Result {
		debug!("Making all users leave the room {room_id} and forgetting it");
		let mut users = self
			.services
			.state_cache
			.room_members(room_id)
			.ready_filter(|user| self.services.globals.user_is_local(user))
			.boxed();

		while let Some(user_id) = users.next().await {
			debug!(
				"Attempting leave for user {user_id} in room {room_id} (ignoring all errors, \
				 evicting admins too)",
			);

			if let Err(e) = self
				.services
				.membership
				.leave(user_id, room_id, Some("Room Deleted".into()), true, &state_lock)
				.boxed()
				.await
			{
				warn!("Failed to leave room: {e}");
			}
		}

		debug!("Deleting all our room aliases for the room");
		self.services
			.alias
			.local_aliases_for_room(room_id)
			.for_each(async |local_alias| {
				self.services
					.alias
					.remove_alias(local_alias, &self.services.globals.server_user)
					.await
					.log_err()
					.ok();
			})
			.await;

		debug!("Removing/unpublishing room from our room directory");
		self.services.directory.set_not_public(room_id);

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

		debug!("Final stages of deleting the room");

		debug!("Deleting room state hash from our database");
		self.services
			.state
			.delete_room_shortstatehash(room_id, &state_lock)
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

		debug!("Successfully deleted room {room_id} from our database");
		Ok(())
	}
}
