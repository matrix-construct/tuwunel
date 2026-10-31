use ruma::{MilliSecondsSinceUnixEpoch, thirdparty::Medium};
use tuwunel_core::{Err, Result, err, utils::BoolExt};
use tuwunel_service::threepid::canonicalize_email;

use crate::{admin_command, utils::parse_active_local_user_id};

#[admin_command]
pub(super) async fn add_email(&self, username: String, address: String) -> Result {
	let user_id = parse_active_local_user_id(self.services, &username).await?;

	if user_id == self.services.globals.server_user {
		return Err!("Not allowed to bind an email address to the server account.");
	}

	let email_canon = canonicalize_email(&address)?;

	let now = MilliSecondsSinceUnixEpoch::now();

	self.services
		.threepid
		.bind_unclaimed(&user_id, &email_canon, Medium::Email, now)
		.await?
		.into_option()
		.ok_or_else(|| err!("Email {email_canon} is already bound to another user."))?;

	write!(self, "Bound email {email_canon} to user {user_id}.").await
}
