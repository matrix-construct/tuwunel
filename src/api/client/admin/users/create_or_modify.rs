use std::collections::BTreeSet;

use axum::extract::State;
use futures::StreamExt;
use ruma::{MilliSecondsSinceUnixEpoch, MxcUri, UserId, thirdparty::Medium};
use synapse_admin_api::users::create_or_modify::v2 as create_or_modify;
use tuwunel_core::{
	Err, Result,
	utils::{IterStream, ReadyExt, stream::automatic_width},
};
use tuwunel_service::{threepid::canonicalize_email, users::PASSWORD_SENTINEL};

use super::user_details;
use crate::{Ruma, client::admin::require_admin};

/// # `PUT /_synapse/admin/v2/users/{user_id}`
///
/// Creates a local account or modifies an existing one. `user_type`,
/// `external_ids` and `approved` are accepted and ignored (not persisted).
pub(crate) async fn admin_create_or_modify_route(
	State(services): State<crate::State>,
	body: Ruma<create_or_modify::Request>,
) -> Result<create_or_modify::Response> {
	let sender_user = body.sender_user();

	require_admin(&services, sender_user).await?;

	let user_id = &body.user_id;

	if !services.globals.user_is_local(user_id) {
		return Err!(Request(InvalidParam("Can only create or modify local users")));
	}

	if body.deactivated == Some(true) && body.locked == Some(true) {
		return Err!(Request(InvalidParam("An account cannot be deactivated and locked")));
	}

	if body.admin == Some(false) && sender_user == body.user_id {
		return Err!(Request(InvalidParam("You may not demote yourself.")));
	}

	let created = !services.users.exists(user_id).await;

	if created {
		services
			.users
			.create(user_id, body.password.as_deref(), None)
			.await?;
	} else if let Some(password) = body.password.as_deref() {
		services
			.users
			.set_password(user_id, Some(password))
			.await?;

		if body.logout_devices {
			services
				.users
				.all_device_ids(user_id)
				.map(ToOwned::to_owned)
				.for_each_concurrent(automatic_width(), async |device_id| {
					services
						.users
						.remove_device(user_id, &device_id)
						.await;
				})
				.await;
		}
	}

	if let Some(displayname) = body.displayname.as_deref() {
		let displayname = (!displayname.is_empty()).then_some(displayname);

		services
			.profile
			.set_displayname(user_id, displayname, None)
			.await?;
	}

	if let Some(avatar_url) = body.avatar_url.as_deref() {
		let avatar_url = (!avatar_url.is_empty()).then(|| <&MxcUri>::from(avatar_url));

		services
			.profile
			.set_avatar_url(user_id, avatar_url, None)
			.await?;
	}

	match body.admin {
		| Some(true) => services.admin.make_user_admin(user_id).await?,
		| Some(false) => services.admin.revoke_admin(user_id).await?,
		| None => {},
	}

	match body.deactivated {
		| Some(true) => services.users.deactivate_account(user_id).await?,
		| Some(false)
			if services
				.users
				.is_deactivated(user_id)
				.await
				.unwrap_or(false) =>
		{
			// Reactivation writes a sentinel so a delegated-auth user can sign in again;
			// a caller supplying a password has already reactivated the account above.
			if body.password.is_none() {
				services
					.users
					.set_password(user_id, Some(PASSWORD_SENTINEL))
					.await?;
			}
		},
		| _ => {},
	}

	match body.locked {
		| Some(true) => services.users.set_locked(user_id, sender_user),
		| Some(false) => services.users.clear_locked(user_id),
		| None => {},
	}

	if let Some(threepids) = body.threepids.as_deref() {
		replace_emails(services, user_id, threepids).await?;
	}

	let details = user_details(services, user_id).await;

	Ok(create_or_modify::Response::new(details))
}

/// Replaces the user's email bindings with exactly the email threepids in
/// `threepids`, canonicalizing each. Non-email media are ignored (no store).
async fn replace_emails(
	services: crate::State,
	user_id: &UserId,
	threepids: &[create_or_modify::ThirdPartyIdentifier],
) -> Result {
	let desired: BTreeSet<String> = threepids
		.iter()
		.filter(|tpid| tpid.medium == Medium::Email)
		.map(|tpid| canonicalize_email(&tpid.address))
		.collect::<Result<_>>()?;

	let current: BTreeSet<String> = services
		.threepid
		.get_bindings(user_id)
		.ready_filter_map(|tpid| (tpid.medium == Medium::Email).then_some(tpid.address))
		.collect()
		.await;

	current
		.difference(&desired)
		.stream()
		.for_each_concurrent(automatic_width(), |address| {
			services.threepid.del_binding(user_id, address)
		})
		.await;

	let now = MilliSecondsSinceUnixEpoch::now();

	desired
		.difference(&current)
		.stream()
		.for_each_concurrent(automatic_width(), |address| {
			services
				.threepid
				.put_binding(user_id, address, Medium::Email, now, now)
		})
		.await;

	Ok(())
}
