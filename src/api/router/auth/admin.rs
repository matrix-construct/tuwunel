use ruma::UserId;
use tuwunel_core::{Result, err};

/// Assert the caller is a server administrator. Generic Synapse admin
/// endpoints use this plain check, not the MSC4323 anti-enumeration
/// `authorize()` guard whose self-target and admin-target ordering does not
/// fit them.
pub(crate) async fn require_admin(services: &crate::State, sender: &UserId) -> Result {
	services
		.admin
		.user_is_admin(sender)
		.await
		.then_some(())
		.ok_or_else(|| {
			err!(Request(Forbidden("Only server administrators can use this endpoint")))
		})
}
