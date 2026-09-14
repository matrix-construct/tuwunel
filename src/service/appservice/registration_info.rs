use ruma::{OwnedUserId, ServerName, UserId, api::appservice::Registration};
use tuwunel_core::Result;

use super::NamespaceRegex;

/// Appservice registration combined with its compiled regular expressions.
#[derive(Clone, Debug)]
pub struct RegistrationInfo {
	pub aliases: NamespaceRegex,
	pub users: NamespaceRegex,
	pub rooms: NamespaceRegex,
	pub sender: OwnedUserId,
	pub registration: Registration,
}

impl RegistrationInfo {
	pub fn new(registration: Registration, server_name: &ServerName) -> Result<Self> {
		Ok(Self {
			aliases: NamespaceRegex::new(false, registration.namespaces.aliases.iter())?,
			users: NamespaceRegex::new(false, registration.namespaces.users.iter())?,
			rooms: NamespaceRegex::new(true, registration.namespaces.rooms.iter())?,
			sender: OwnedUserId::from_parts(
				'@',
				registration.sender_localpart.as_ref(),
				server_name.as_str().into(),
			)?,

			registration,
		})
	}

	/// MSC3905: the `users` regex matches local users only.
	#[inline]
	#[must_use]
	pub fn is_user_match(&self, user_id: &UserId) -> bool {
		user_id == self.sender
			|| (self.users.is_match(user_id.as_str())
				&& user_id.server_name() == self.sender.server_name())
	}

	/// MSC3905: the `users` regex matches local users only.
	#[inline]
	#[must_use]
	pub fn is_exclusive_user_match(&self, user_id: &UserId) -> bool {
		user_id == self.sender
			|| (self.users.is_exclusive_match(user_id.as_str())
				&& user_id.server_name() == self.sender.server_name())
	}
}
