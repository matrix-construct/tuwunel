use futures::FutureExt;
use ruma::events::room::{
	guest_access::GuestAccess, join_rules::JoinRule, preview_url::RoomPreviewUrlsEventContent,
};
use tuwunel_core::{Result, implement, pdu::PduBuilder};

use super::Service;

#[implement(Service)]
async fn create_server_user(&self) -> Result {
	let server_user = self.services.globals.server_user.as_ref();
	if !self.services.users.exists(server_user).await {
		self.services
			.users
			.create(server_user, None, None)
			.await?;
	}

	Ok(())
}

/// Create the admin room.
///
/// Users in this room are considered admins by tuwunel, and the room can be
/// used to issue admin commands by talking to the server user inside it.
#[implement(Service)]
pub async fn create_admin_room(&self) -> Result {
	self.create_server_user().await?;

	let server_user = self.services.globals.server_user.as_ref();

	let name = format!("{} Admin Room", self.services.config.server_name);
	let topic = format!(
		"Manage {} | Run commands prefixed with `!admin` | Run `!admin -h` for help | Documentation: https://github.com/matrix-construct/tuwunel/",
		self.services.config.server_name
	);

	self.services
		.create
		.create_room(
			server_user,
			None,
			None,
			Some(&self.admin_alias),
			&[],
			false,
			vec![PduBuilder::state(String::new(), &RoomPreviewUrlsEventContent {
				disabled: true,
			})],
			JoinRule::Invite,
			GuestAccess::Forbidden,
			false,
			Some(&name),
			Some(&topic),
			None,
			None,
		)
		.boxed()
		.await?;

	Ok(())
}
