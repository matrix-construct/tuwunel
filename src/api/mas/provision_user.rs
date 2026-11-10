use std::collections::BTreeSet;

use axum::extract::State;
use futures::{StreamExt, TryStreamExt};
use ruma::{MilliSecondsSinceUnixEpoch, UserId, profile::ProfileFieldName, thirdparty::Medium};
use synapse_admin_api::mas::provision_user::{Request, Response};
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	utils::{
		IterStream, ReadyExt,
		option::OptionExt,
		stream::{TryBroadbandExt, automatic_width},
		string::truncate_chars,
	},
	warn,
};
use tuwunel_service::{
	profile::{MAX_DISPLAYNAME_LENGTH, ProfileValue, string_field},
	threepid::canonicalize_email,
	users::PASSWORD_SENTINEL,
};

use super::{Mas, local_user};
use crate::Ruma;

type ProfileWrites = ArrayVec<ProfileValue, 2>;

/// # `POST /_synapse/mas/provision_user`
pub(crate) async fn provision_user_route(
	_mas: Mas,
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let user_id = local_user(services, &body.localpart)?;

	// Canonicalize up front so a malformed address fails before any mutation.
	let desired_emails: Option<BTreeSet<String>> = if body.unset_emails {
		Some(BTreeSet::new())
	} else {
		body.set_emails
			.as_deref()
			.map(canonicalize_emails)
			.transpose()?
	};

	let created = !services.users.exists(&user_id).await;
	let profile = requested_profile(services, &user_id, &body).await;

	let account = create_account(services, &user_id, created);

	// A refused profile value returns before the account exists.
	services
		.profile
		.set_profile_keys_after(&user_id, &profile, None, account)
		.await?;

	if let Some(desired) = desired_emails {
		sync_emails(services, &user_id, desired).await?;
	}

	match body.locked {
		| Some(true) => services
			.users
			.set_locked(&user_id, &services.globals.server_user),
		| Some(false) => services.users.clear_locked(&user_id),
		| None => {},
	}

	Ok(Response::new(created))
}

fn canonicalize_emails(addrs: &[String]) -> Result<BTreeSet<String>> {
	addrs
		.iter()
		.map(|a| canonicalize_email(a))
		.collect()
}

/// The display name and avatar URL the request sets or unsets.
///
/// MAS relays the upstream provider's display name, which the user may have no
/// way to shorten there, so an overlong one is cut to the profile's limit
/// rather than refused.
async fn requested_profile(
	services: crate::State,
	user_id: &UserId,
	body: &Request,
) -> ProfileWrites {
	let displayname = body
		.set_displayname
		.as_deref()
		.map_async(|name| fitted_displayname(services, user_id, name))
		.await;

	let touch_displayname = displayname.is_some() || body.unset_displayname;
	let touch_avatar = body.set_avatar_url.is_some() || body.unset_avatar_url;
	let avatar_url = body.set_avatar_url.as_deref();

	[
		touch_displayname.then(|| string_field(ProfileFieldName::DisplayName, displayname)),
		touch_avatar.then(|| string_field(ProfileFieldName::AvatarUrl, avatar_url)),
	]
	.into_iter()
	.flatten()
	.collect()
}

// A name equal to the stored one is restated whole, as the profile accepts it.
async fn fitted_displayname<'a>(
	services: crate::State,
	user_id: &UserId,
	name: &'a str,
) -> &'a str {
	let fitted = truncate_chars(name, MAX_DISPLAYNAME_LENGTH);

	if fitted.len() == name.len() {
		return name;
	}

	let restated = services
		.profile
		.displayname(user_id)
		.await
		.is_ok_and(|stored| stored == name);

	if restated { name } else { fitted }
}

#[tracing::instrument(level = "debug", skip_all)]
async fn create_account(services: crate::State, user_id: &UserId, created: bool) -> Result {
	if !created {
		return Ok(());
	}

	services
		.users
		.create(user_id, Some(PASSWORD_SENTINEL), Some("oidc"))
		.await
}

/// Reconcile the user's bound email addresses to exactly `desired`. A desired
/// address bound to another user is reassigned, its prior binding removed first
/// so no dangling forward row survives.
async fn sync_emails(
	services: crate::State,
	user_id: &UserId,
	desired: BTreeSet<String>,
) -> Result {
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
		.try_stream()
		.broad_and_then(async |address| {
			if let Some(prior) = services
				.threepid
				.user_id_for_email(address)
				.await?
				&& prior != user_id
			{
				warn!(%user_id, %prior, "MAS provisioned an email bound to another user; reassigning");

				services
					.threepid
					.del_binding(&prior, address)
					.await;
			}

			services
				.threepid
				.put_binding(user_id, address, Medium::Email, now, now)
				.await;

			Ok(())
		})
		.try_collect::<()>()
		.await
}
