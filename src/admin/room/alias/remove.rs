use tuwunel_core::{Result, err};

use super::parse_alias_from_localpart;
use crate::admin_command;

#[admin_command]
pub(super) async fn alias_remove(&self, room_alias_localpart: String) -> Result {
	let room_alias = parse_alias_from_localpart(self.services, &room_alias_localpart)?;

	// remove_alias fails only when the alias is absent or its value is invalid.
	let room_id = self
		.services
		.alias
		.remove_alias(&room_alias)
		.await
		.map_err(|_| err!("Alias isn't in use."))?;

	write!(self, "Removed alias from {room_id}").await
}
