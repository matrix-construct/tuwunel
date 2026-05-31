use std::collections::BTreeSet;

use axum::extract::State;
use futures::{StreamExt, TryStreamExt};
use ruma::{MilliSecondsSinceUnixEpoch, MxcUri, UserId, thirdparty::Medium};
use synapse_admin_api::mas::provision_user::{Request, Response};
use tuwunel_core::{
	Result,
	utils::{
		IterStream, ReadyExt,
		stream::{TryBroadbandExt, automatic_width},
	},
	warn,
};
use tuwunel_service::{threepid::canonicalize_email, users::PASSWORD_SENTINEL};

use super::{Mas, local_user};
use crate::Ruma;

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

	if created {
		services
			.users
			.create(&user_id, Some(PASSWORD_SENTINEL), Some("oidc"))
			.await?;
	}

	let touch_displayname = body.set_displayname.is_some() || body.unset_displayname;
	let touch_avatar = body.set_avatar_url.is_some() || body.unset_avatar_url;
	if touch_displayname || touch_avatar {
		if touch_displayname {
			services
				.profile
				.set_displayname(&user_id, body.set_displayname.as_deref(), None)
				.await?;
		}

		if touch_avatar {
			let avatar = body
				.set_avatar_url
				.as_deref()
				.map(<&MxcUri>::from);

			services
				.profile
				.set_avatar_url(&user_id, avatar, None)
				.await?;
		}
	}

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
				.await? && prior != user_id
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
