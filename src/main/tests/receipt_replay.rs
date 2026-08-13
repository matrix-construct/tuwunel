#![cfg(test)]

use std::{
	collections::BTreeMap, env::var, fs::remove_dir_all, path::PathBuf, process::id as process_id,
};

use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Result,
	ruma::{
		EventId, MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId, UserId, event_id,
		events::receipt::{
			Receipt, ReceiptEvent, ReceiptEventContent, ReceiptThread, ReceiptType,
		},
		room_id, user_id,
	},
	utils::ReadyExt,
};
use tuwunel_service::{Services, rooms::read_receipt::PrivateRead};

const PRIVATE_READ_MIRROR: &str = "roomuserid_privatereadsync";

struct DatabasePath(PathBuf);

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}

#[test]
fn replayed_receipts_hold_their_stream_position() -> Result {
	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = DatabasePath(
		PathBuf::from(root).join(format!("tuwunel-receipt-replay-{}", process_id())),
	);

	let mut args = Args::default_test(&["fresh", "cleanup"]);
	args.maintenance = true;
	args.option
		.push(format!("database_path={:?}", db_path.0));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let outcome = exercise(&services).await;
		let shutdown = server.server.shutdown();

		drop(services);

		let run = async_run(&server).await;
		let stop = async_stop(&server).await;

		outcome.and(shutdown).and(run).and(stop)
	});

	drop(runtime);

	result
}

async fn exercise(services: &Services) -> Result {
	let room = room_id!("!receipt-replay:localhost");
	let user = user_id!("@receipt-replay:localhost");
	let receipts = &services.read_receipt;
	let first = receipt_event(room, user, event_id!("$receipt-replay-first:localhost"));
	let second = receipt_event(room, user, event_id!("$receipt-replay-second:localhost"));

	let stored = receipts
		.readreceipt_update(user, room, &first)
		.await;

	let after_store = receipt_counts(services, room).await;

	let replayed = receipts
		.readreceipt_update(user, room, &first)
		.await;

	let after_replay = receipt_counts(services, room).await;
	let advanced = receipts
		.readreceipt_update(user, room, &second)
		.await;

	let after_advance = receipt_counts(services, room).await;

	if !stored || after_store.len() != 1 {
		return Err!("first receipt did not store exactly one row");
	}

	if replayed || after_replay != after_store {
		return Err!("replayed receipt moved the receipt stream");
	}

	if !advanced || after_advance.len() != 1 || after_advance[0] <= after_store[0] {
		return Err!("receipt naming another event did not take a new position");
	}

	private_read_replay(services, room, user).await
}

fn receipt_event(room: &RoomId, user: &UserId, event: &EventId) -> ReceiptEvent {
	let receipt = Receipt {
		ts: Some(MilliSecondsSinceUnixEpoch::now()),
		thread: ReceiptThread::Unthreaded,
	};

	let users = BTreeMap::from([(user.to_owned(), receipt)]);
	let read = BTreeMap::from([(ReceiptType::Read, users)]);
	let content = ReceiptEventContent(BTreeMap::from([(event.to_owned(), read)]));

	ReceiptEvent { content, room_id: room.to_owned() }
}

async fn receipt_counts(services: &Services, room: &RoomId) -> Vec<u64> {
	services
		.read_receipt
		.readreceipts_since(room, 0, None)
		.ready_fold(Vec::new(), |mut counts, (_, count, _)| {
			counts.push(count);
			counts
		})
		.await
}

async fn private_read_replay(services: &Services, room: &RoomId, user: &UserId) -> Result {
	let token = async || {
		services
			.read_receipt
			.last_privateread_update(user, room)
			.await
	};

	let stored = set_private_read(services, room, user, 5, true).await;
	let after_store = token().await;
	let replayed = set_private_read(services, room, user, 5, true).await;
	let earlier = set_private_read(services, room, user, 4, true).await;
	let after_replay = token().await;
	let advanced = set_private_read(services, room, user, 6, true).await;
	let after_advance = token().await;

	if !stored || !advanced {
		return Err!("private marker rejected an advancing position");
	}

	if replayed || earlier || after_replay != after_store {
		return Err!("private marker accepted a position it already held");
	}

	if after_advance <= after_store {
		return Err!("advancing private marker did not move the update token");
	}

	private_read_unannounced(services, room, user).await
}

/// The append path marks a sender's own send read without publishing it, so an
/// unannounced marker must advance the stored position while leaving the sync
/// gate where the client last set it.
async fn private_read_unannounced(services: &Services, room: &RoomId, user: &UserId) -> Result {
	let before = services
		.read_receipt
		.last_privateread_update(user, room)
		.await;

	let (published_before, _) = services
		.read_receipt
		.private_read_sync_get_count(room, user)
		.await?;

	let stored = set_private_read(services, room, user, 7, false).await;
	let after = services
		.read_receipt
		.last_privateread_update(user, room)
		.await;

	let (position, _) = services
		.read_receipt
		.private_read_get_count(room, user)
		.await?;

	let (published_after, _) = services
		.read_receipt
		.private_read_sync_get_count(room, user)
		.await?;

	if !stored || position != 7 {
		return Err!("unannounced private marker did not store its position");
	}

	if after != before {
		return Err!("unannounced private marker moved the update token");
	}

	if published_after != published_before {
		return Err!("unannounced private marker changed the announced snapshot");
	}

	let announced = set_private_read(services, room, user, 8, true).await;
	let (published, _) = services
		.read_receipt
		.private_read_sync_get_count(room, user)
		.await?;

	if !announced || published != 8 {
		return Err!("announced private marker did not advance the sync snapshot");
	}

	private_read_premirror(services, room, user).await
}

/// A database predating the announced mirror holds gate rows with no mirror
/// rows.
///
/// The bounded reader must fall back to the active state rather than fail the
/// room range until the next announced write. A marker naming a resolvable
/// event is served; one naming an unknown event is skipped, not an error.
async fn private_read_premirror(services: &Services, room: &RoomId, user: &UserId) -> Result {
	// Prime the short room id so the fallible reader can resolve the room.
	services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let gate = services
		.read_receipt
		.last_privateread_update(user, room)
		.await;

	let mirror = &services.db[PRIVATE_READ_MIRROR];

	mirror.del((room, user));
	mirror.del((room, user, ""));

	let events = services
		.read_receipt
		.private_read_get_fallible(room, user, gate)
		.await?;

	if !events.is_empty() {
		return Err!("pre-mirror fallback should skip markers without a resolvable event");
	}

	let (admin_room, position) = admin_room_head(services).await?;

	if !set_private_read(services, &admin_room, user, position, true).await {
		return Err!("admin-room private marker did not store");
	}

	let gate = services
		.read_receipt
		.last_privateread_update(user, &admin_room)
		.await;

	mirror.del((&admin_room, user));
	mirror.del((&admin_room, user, ""));

	let served = services
		.read_receipt
		.private_read_get_fallible(&admin_room, user, gate)
		.await?;

	if served.len() != 1 {
		return Err!("pre-mirror fallback did not serve the active marker");
	}

	private_read_dangling(services, room, user).await
}

async fn private_read_dangling(services: &Services, room: &RoomId, user: &UserId) -> Result {
	if !set_private_read(services, room, user, 9, true).await {
		return Err!("announced marker naming a fake event did not store");
	}

	let gate = services
		.read_receipt
		.last_privateread_update(user, room)
		.await;

	let empty = services
		.read_receipt
		.private_read_get_fallible(room, user, gate)
		.await?;

	if !empty.is_empty() {
		return Err!("a fully dangling mirror did not serve an empty snapshot");
	}

	let dangler = user_id!("@receipt-dangling:localhost");
	let (admin_room, position) = admin_room_head(services).await?;

	if !set_private_read(services, &admin_room, dangler, position, true).await {
		return Err!("private marker naming a real event did not store");
	}

	// A position no PDU occupies; the global counter is nowhere near it.
	let stored = services
		.read_receipt
		.private_read_set(PrivateRead {
			room_id: &admin_room,
			user_id: dangler,
			count: u64::MAX / 2,
			ts: MilliSecondsSinceUnixEpoch::now(),
			thread: &ReceiptThread::Main,
			announce: true,
		})
		.await;

	if !stored {
		return Err!("private marker naming a missing event did not store");
	}

	let update = services
		.read_receipt
		.last_privateread_update(dangler, &admin_room)
		.await;

	let served = services
		.read_receipt
		.private_read_get_fallible(&admin_room, dangler, update)
		.await?;

	if served.len() != 1 {
		return Err!("dangling marker was not skipped from the announced snapshot");
	}

	if !served[0].json().get().contains("m.read.private") {
		return Err!("served snapshot is not a private read receipt");
	}

	let stale = services
		.read_receipt
		.private_read_get_fallible(&admin_room, dangler, update.saturating_sub(1))
		.await;

	if !stale.is_err_and(|error| !error.is_not_found()) {
		return Err!("stale update token did not fail the range");
	}

	let mirror = &services.db[PRIVATE_READ_MIRROR];

	mirror.put((&admin_room, dangler, "not-a-thread"), (1_u64, 1_u64));

	let undecodable = services
		.read_receipt
		.private_read_get_fallible(&admin_room, dangler, update)
		.await;

	mirror.del((&admin_room, dangler, "not-a-thread"));

	if !undecodable.is_err_and(|error| !error.is_not_found()) {
		return Err!("undecodable mirror row did not fail the range");
	}

	Ok(())
}

async fn set_private_read(
	services: &Services,
	room: &RoomId,
	user: &UserId,
	count: u64,
	announce: bool,
) -> bool {
	services
		.read_receipt
		.private_read_set(PrivateRead {
			room_id: room,
			user_id: user,
			count,
			ts: MilliSecondsSinceUnixEpoch::now(),
			thread: &ReceiptThread::Unthreaded,
			announce,
		})
		.await
}

async fn admin_room_head(services: &Services) -> Result<(OwnedRoomId, u64)> {
	let room = services.admin.get_admin_room().await?;
	let position = services
		.timeline
		.last_timeline_count(None, &room, None)
		.await?
		.into_unsigned();

	Ok((room, position))
}
