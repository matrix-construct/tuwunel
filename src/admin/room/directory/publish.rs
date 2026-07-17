use std::borrow::Cow;

use ruma::{OwnedRoomOrAliasId, RoomAliasId};
use tuwunel_core::Result;

use crate::admin_command;

#[admin_command]
pub(super) async fn directory_publish(&self, room: OwnedRoomOrAliasId, force: bool) -> Result {
	let room_id = self.services.alias.maybe_resolve(&room).await?;

	let alias = <&RoomAliasId>::try_from(&*room)
		.ok()
		.filter(|_| !force);

	let local_alias = alias.filter(|alias| self.services.globals.alias_is_local(alias));

	self.services
		.directory
		.set_public(&room_id, local_alias);

	let out = match (alias, local_alias) {
		| (None, _) => Cow::Borrowed("Room published"),
		| (Some(alias), Some(_)) => format!("Room published as {alias}").into(),
		| (Some(alias), None) =>
			format!("Room published; remote alias {alias} was not recorded").into(),
	};

	self.write_str(&out).await
}
