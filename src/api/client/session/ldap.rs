use futures::FutureExt;
use ruma::{OwnedUserId, UserId};
use tuwunel_core::{Err, Result, debug, error, info, warn};
use tuwunel_service::Services;

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
			.create(lowercased_user_id, Some("*"), Some("ldap"))
			.await?;

		// Auto-join rooms for newly created LDAP users
		if !services.server.config.auto_join_rooms.is_empty() {
			for room in &services.server.config.auto_join_rooms {
				let Ok(room_id) = services.alias.maybe_resolve(room).await else {
					error!(
						"Failed to resolve room alias to room ID when attempting to auto join \
						 {room}, skipping"
					);
					continue;
				};

				if !services
					.state_cache
					.server_in_room(services.globals.server_name(), &room_id)
					.await
				{
					warn!(
						"Skipping room {room} to automatically join as we have never joined \
						 before."
					);
					continue;
				}

				if let Some(room_server_name) = room.server_name() {
					let state_lock = services.state.mutex.lock(&room_id).await;

					match services
						.membership
						.join(
							lowercased_user_id,
							&room_id,
							Some("Automatically joining this room upon first login".to_owned()),
							&[
								services.globals.server_name().to_owned(),
								room_server_name.to_owned(),
							],
							&None,
							&state_lock,
						)
						.boxed()
						.await
					{
						| Err(e) => {
							// don't return this error so we don't fail logins
							error!(
								"Failed to automatically join room {room} for user \
								 {lowercased_user_id}: {e}"
							);
						},
						| _ => {
							info!(
								"Automatically joined room {room} for user {lowercased_user_id}"
							);
						},
					}

					drop(state_lock);
				}
			}
		}
	}

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

	Ok(user_id)
}
