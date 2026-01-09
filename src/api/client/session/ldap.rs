use futures::FutureExt;
use ruma::{OwnedUserId, UserId};
use tuwunel_core::{Err, Result, debug};
use tuwunel_service::{Services, users::Register};

use super::password_login;

/// Authenticates the given user through the configured LDAP server.
///
/// Creates the user if the user is found in the LDAP and do not already have an
/// account.
#[tracing::instrument(skip_all, fields(%user_id), name = "ldap")]
pub(super) async fn ldap_login(
	services: &Services,
	user_id: &UserId,
	lowercased_user_id: &UserId,
	password: &str,
) -> Result<OwnedUserId> {
	let (user_dn, is_ldap_admin) = match services.config.ldap.bind_dn.as_ref() {
		| Some(bind_dn) if bind_dn.contains("{username}") =>
			(bind_dn.replace("{username}", lowercased_user_id.localpart()), false),
		| _ => {
			debug!("Searching user in LDAP");

			let dns = services.users.search_ldap(user_id).await?;
			if dns.len() >= 2 {
				return Err!(Ldap("LDAP search returned two or more results"));
			}

			let Some((user_dn, is_admin)) = dns.first() else {
				return password_login(services, user_id, lowercased_user_id, password).await;
			};

			(user_dn.clone(), *is_admin)
		},
	};

	let user_id = services
		.users
		.auth_ldap(&user_dn, password)
		.await
		.map(|()| lowercased_user_id.to_owned())?;

	// LDAP users are automatically created on first login attempt. This is a very
	// common feature that can be seen on many services using a LDAP provider for
	// their users (synapse, Nextcloud, Jellyfin, ...).
	//
	// LDAP users are crated with a dummy password but non empty because an empty
	// password is reserved for deactivated accounts. The tuwunel password field
	// will never be read to login a LDAP user so it's not an issue.
	if !services.users.exists(lowercased_user_id).await {
		services
			.users
			.full_register(Register {
				user_id: Some(lowercased_user_id),
				password: Some("*"),
				origin: Some("ldap"),
				..Default::default()
			})
			.await?;
	}

	// only perform admin add/remove check if admin_filter is set
	if !services.config.ldap.admin_filter.is_empty() {
		let is_tuwunel_admin = services
			.admin
			.user_is_admin(lowercased_user_id)
			.await;

		if is_ldap_admin && !is_tuwunel_admin {
			services
				.admin
				.make_user_admin(lowercased_user_id)
				.boxed()
				.await?;
		} else if !is_ldap_admin && is_tuwunel_admin {
			services
				.admin
				.revoke_admin(lowercased_user_id)
				.await?;
		}
	}

	Ok(user_id)
}
