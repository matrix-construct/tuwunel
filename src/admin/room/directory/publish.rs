use std::borrow::Cow;

use ruma::OwnedRoomOrAliasId;
use tuwunel_core::{Err, Result};

use super::local_alias;
use crate::admin_command;

#[admin_command]
pub(super) async fn directory_publish(&self, room: OwnedRoomOrAliasId, force: bool) -> Result {
	let alias = local_alias(self.services, &room)?.filter(|_| !force);

	let room_id = self.services.alias.maybe_resolve(&room).await?;

	if !force && !self.services.metadata.exists(&room_id).await {
		return Err!(
			"Room {room_id} is not known to this server; use the force flag to publish anyway"
		);
	}

	self.services
		.directory
		.set_public(&room_id, alias);

	let out = match alias {
		| None => Cow::Borrowed("Room published"),
		| Some(alias) => format!("Room published as {alias}").into(),
	};

	self.write_str(&out).await
}
