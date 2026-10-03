use futures::TryStreamExt;
use ruma::OwnedRoomOrAliasId;
use tuwunel_core::{Result, utils::stream::TryTools};
use tuwunel_matrix::PduCount;

use crate::admin_command;

#[admin_command]
pub(super) async fn pdus(
	&self,
	room_id: OwnedRoomOrAliasId,
	from: Option<String>,
	limit: Option<usize>,
) -> Result {
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	let from: Option<PduCount> = from.as_deref().map(str::parse).transpose()?;

	let result: Vec<_> = self
		.services
		.timeline
		.pdus_rev(None, &room_id, from)
		.try_take(limit.unwrap_or(3))
		.try_collect()
		.await?;

	write!(self, "{result:#?}").await
}
