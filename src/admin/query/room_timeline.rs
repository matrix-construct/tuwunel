use clap::Subcommand;
use futures::TryStreamExt;
use ruma::OwnedRoomOrAliasId;
use tuwunel_core::{PduCount, Result, utils::stream::TryTools};

use crate::{admin_command, admin_command_dispatch};

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
/// Query tables from database
pub(crate) enum RoomTimelineCommand {
	Pdus {
		room_id: OwnedRoomOrAliasId,

		from: Option<String>,

		#[arg(short, long)]
		limit: Option<usize>,
	},

	Last {
		room_id: OwnedRoomOrAliasId,
	},
}

#[admin_command]
pub(super) async fn last(&self, room_id: OwnedRoomOrAliasId) -> Result {
	let room_id = self
		.services
		.rooms
		.alias
		.resolve(&room_id)
		.await?;

	let result = self
		.services
		.rooms
		.timeline
		.last_timeline_count(None, &room_id, None)
		.await?;

	self.write_str(&format!("{result:#?}")).await
}

#[admin_command]
pub(super) async fn pdus(
	&self,
	room_id: OwnedRoomOrAliasId,
	from: Option<String>,
	limit: Option<usize>,
) -> Result {
	let room_id = self
		.services
		.rooms
		.alias
		.resolve(&room_id)
		.await?;

	let from: Option<PduCount> = from.as_deref().map(str::parse).transpose()?;

	let result: Vec<_> = self
		.services
		.rooms
		.timeline
		.pdus_rev(None, &room_id, from)
		.try_take(limit.unwrap_or(3))
		.try_collect()
		.await?;

	self.write_str(&format!("{result:#?}")).await
}
