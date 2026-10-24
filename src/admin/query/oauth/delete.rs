use ruma::UserId;
use tuwunel_core::{
	Err, Result,
	either::{Left, Right},
	implement,
};
use tuwunel_service::oauth::{Deleted, Deletions};

use super::SessionOrUserId;
use crate::admin_command;

#[admin_command]
pub(super) async fn oauth_delete(&self, id: SessionOrUserId, force: bool) -> Result {
	if !force {
		return Err!(
			"Deleting these records can cause registration conflicts. Use --force to be sure."
		);
	}

	match id {
		| Left(sess_id) => self.oauth_delete_session(&sess_id).await,
		| Right(user_id) => self.oauth_delete_user(&user_id).await,
	}
}

#[implement(crate::Context, params = "<'_>")]
async fn oauth_delete_session(&self, sess_id: &str) -> Result {
	let deleted = self
		.services
		.oauth
		.sessions
		.delete(sess_id)
		.await?;

	let residue = match deleted {
		| Deleted::Complete => "",
		| Deleted::ProviderUnresolved =>
			" No configured provider derives its identity key, so an identity binding to it may \
			 remain; sign-in treats such a binding as unlinked.",
	};

	writeln!(self, "Deleted OAuth session {sess_id}.{residue}").await
}

#[implement(crate::Context, params = "<'_>")]
async fn oauth_delete_user(&self, user_id: &UserId) -> Result {
	let deletions = self
		.services
		.oauth
		.delete_user_sessions(user_id)
		.await?;

	let deleted = deletions.deleted();
	let Deletions { unresolved, absent, unreadable, .. } = deletions;

	writeln!(self, "Deleted {deleted} OAuth session(s) for {user_id}.").await?;

	if unresolved > 0 {
		writeln!(
			self,
			"{unresolved} of them have no configured provider to derive their identity key, so \
			 an identity binding to them may remain; sign-in treats such a binding as unlinked."
		)
		.await?;
	}

	if absent > 0 {
		writeln!(self, "{absent} listed session(s) had no stored record.").await?;
	}

	if unreadable > 0 {
		writeln!(self, "{unreadable} session(s) could not be read and were kept; see the log.")
			.await?;
	}

	Ok(())
}
