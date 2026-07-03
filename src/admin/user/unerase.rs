use tuwunel_core::Result;

use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn unerase(&self, user_id: String) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;

	self.services.users.clear_erased(&user_id);

	write!(self, "Cleared the erasure marker of {user_id}").await
}
