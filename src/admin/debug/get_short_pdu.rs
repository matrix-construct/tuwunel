use tuwunel_core::{Result, err};
use tuwunel_matrix::pdu::{PduId, RawPduId};
use tuwunel_service::rooms::short::ShortRoomId;

use crate::admin_command;

#[admin_command]
pub(super) async fn get_short_pdu(&self, shortroomid: ShortRoomId, count: i64) -> Result {
	let pdu_id: RawPduId = PduId { shortroomid, count: count.into() }.into();

	let pdu_json = self
		.services
		.timeline
		.get_pdu_json_from_id(&pdu_id)
		.await;

	let json = pdu_json.map_err(|_| err!("PDU not found locally."))?;

	let json_text = serde_json::to_string_pretty(&json)?;

	write!(self, "```json\n{json_text}\n```").await
}
