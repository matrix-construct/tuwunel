mod support;

#[cfg(test)]
mod tests {
	use serde_json::json;
	use tuwunel_core::{
		Result,
		ruma::{events::RoomAccountDataEventType, user_id},
	};
	use tuwunel_service::users::{DeactivationReason, PASSWORD_SENTINEL};

	use super::support::Harness;

	#[test]
	fn self_service_erase_deactivation_keeps_sso_reactivation_path() -> Result {
		let harness = Harness::new("mindroom_rebase_deactivate", [])?;

		harness.with_services(|services| async move {
			let user_id = user_id!("@alice:localhost");
			let account_data_type = RoomAccountDataEventType::Tag;
			let account_data = json!({
				"type": account_data_type.to_string(),
				"content": {
					"tags": {
						"u.work": { "order": 0.1 }
					}
				}
			});

			services
				.users
				.create(user_id, Some(PASSWORD_SENTINEL), Some("sso"))
				.await?;
			services
				.account_data
				.update(None, user_id, account_data_type.clone(), &account_data)
				.await?;
			assert!(
				services
					.account_data
					.get_raw(None, user_id, account_data_type.to_string().as_str())
					.await
					.is_ok(),
				"test setup should store account data before deactivation",
			);

			services
				.deactivate
				.full_deactivate(user_id, true, DeactivationReason::SelfService)
				.await?;

			assert!(
				services
					.account_data
					.get_raw(None, user_id, account_data_type.to_string().as_str())
					.await
					.is_err(),
				"MSC4025 erase=true should remove global account data",
			);

			let reactivated = services
				.users
				.maybe_reactivate_deactivated_sso(user_id)
				.await?;
			assert!(
				reactivated,
				"self-service deactivation reason should still allow SSO reactivation after \
				 erase",
			);
			assert!(
				!services.users.is_deactivated(user_id).await?,
				"reactivated SSO user should no longer be deactivated",
			);
			assert_eq!(
				services.users.password_hash(user_id).await?,
				PASSWORD_SENTINEL,
				"SSO reactivation should restore the sentinel password",
			);

			Ok(())
		})
	}
}
