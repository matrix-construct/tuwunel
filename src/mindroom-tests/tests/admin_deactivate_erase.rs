mod support;

#[cfg(test)]
mod tests {
	use tuwunel_core::{Result, ruma::user_id};
	use tuwunel_service::users::{DeactivationReason, PASSWORD_SENTINEL};

	use super::support::Harness;

	/// `POST /_synapse/mas/delete_user` (new upstream endpoint) performs
	/// `full_deactivate(user, erase, DeactivationReason::Admin)`. Unlike
	/// self-service deactivation, this must never leave an SSO reactivation
	/// path open — an account deleted by an administrator stays deactivated
	/// when the same SSO identity logs in again.
	#[test]
	fn admin_erase_deactivation_blocks_sso_reactivation() -> Result {
		let harness = Harness::new("mindroom_rebase_admin_deactivate", [])?;

		harness.with_services(|services| async move {
			let user_id = user_id!("@mallory:localhost");

			services
				.users
				.create(user_id, Some(PASSWORD_SENTINEL), Some("sso"))
				.await?;

			services
				.deactivate
				.full_deactivate(user_id, true, DeactivationReason::Admin)
				.await?;

			assert!(
				services.users.is_deactivated(user_id).await?,
				"admin deactivation should deactivate the account",
			);

			let reactivated = services
				.users
				.maybe_reactivate_deactivated_sso(user_id)
				.await?;
			assert!(
				!reactivated,
				"admin-erased deactivation must not be reversible by SSO login",
			);
			assert!(
				services.users.is_deactivated(user_id).await?,
				"account must remain deactivated after an SSO reactivation attempt",
			);

			Ok(())
		})
	}
}
