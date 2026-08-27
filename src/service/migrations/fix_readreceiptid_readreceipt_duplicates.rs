use ruma::{RoomId, UserId, identifiers_validation::ID_MAX_BYTES};
use tuwunel_core::{
	Result,
	arrayvec::ArrayString,
	info,
	utils::{ReadyExt, stream::TryExpect},
	warn,
};

use crate::Services;

type ArrayId = ArrayString<ID_MAX_BYTES>;
type Key<'a> = (&'a RoomId, u64, &'a UserId);

pub(super) async fn fix_readreceiptid_readreceipt_duplicates(services: &Services) -> Result {
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
			services.server.progress.advance();

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
	readreceiptid_readreceipt.sort()
}
