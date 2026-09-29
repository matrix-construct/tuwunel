use futures::FutureExt;
use tuwunel_core::Result;

use crate::{admin_command, utils::parse_existing_local_user_id};

#[admin_command]
pub(super) async fn make_user_admin(&self, user_id: String) -> Result {
	let user_id = parse_existing_local_user_id(self.services, &user_id).await?;

	assert!(
		self.services.globals.user_is_local(&user_id),
		"Parsed user_id must be a local user"
	);

	self.services
		.admin
		.make_user_admin(&user_id)
		.boxed()
		.await?;

	write!(self, "{user_id} has been granted admin privileges.").await
}
