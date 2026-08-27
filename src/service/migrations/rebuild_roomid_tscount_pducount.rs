use ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId};
use serde::Deserialize;
use tuwunel_core::{
	Result, info,
	matrix::pdu::RawPduId,
	utils::{ReadyExt, stream::TryIgnore},
	warn,
};

use crate::{Services, rooms::timeline::bias_count};

#[derive(Deserialize)]
struct PduRoomTs {
	room_id: OwnedRoomId,
	origin_server_ts: MilliSecondsSinceUnixEpoch,
}

pub(super) async fn rebuild_roomid_tscount_pducount(services: &Services) -> Result {
	let db = &services.db;
	let cork = db.cork_and_sync();
	let pduid_pdu = db["pduid_pdu"].clone();
	let roomid_tscount_pducount = db["roomid_tscount_pducount"].clone();

	warn!("Rebuilding roomid_tscount_pducount index for same-timestamp event ordering");

	let count = pduid_pdu
		.raw_stream()
		.ignore_err()
		.ready_fold(0_usize, |count, (key, value)| {
			services.server.progress.advance();

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
	roomid_tscount_pducount.sort()
}
