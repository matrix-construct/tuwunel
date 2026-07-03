use tuwunel_core::Result;

use crate::{admin_command, utils::parse_local_user_id};

#[admin_command]
pub(super) async fn erasure(&self, user_id: String) -> Result {
	let user_id = parse_local_user_id(self.services, &user_id)?;

	match self.services.users.erasure_count(&user_id).await {
		| Some(count) => write!(self, "{user_id} is erased (since count {count})").await,
		| None => write!(self, "{user_id} is not erased").await,
	}
}
