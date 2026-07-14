use axum::extract::State;
use futures::StreamExt;
use ruma::{MilliSecondsSinceUnixEpoch, UInt, UserId, api::Direction};
use synapse_admin_api::users::list_users::{
	v2::{self, UserMinorDetails},
	v3,
};
use tuwunel_core::{
	Result,
	utils::{
		IterStream, ReadyExt,
		math::{ruma_from_usize, usize_from_ruma},
		stream::WidebandExt,
	},
};

use crate::{Ruma, client::admin::require_admin};

/// The `deactivated` query filter, whose semantics differ between v2 and v3.
#[derive(Clone, Copy)]
enum DeactivatedFilter {
	/// Include both active and deactivated users (v3 absent).
	Any,

	/// Only deactivated users (v3 `true`).
	Only,

	/// Exclude deactivated users (v2 absent/false, v3 `false`).
	Exclude,

	/// Include deactivated users (v2 `true`).
	Include,
}

/// Filter and pagination parameters shared by the v2 and v3 list endpoints.
struct ListParams<'a> {
	from: usize,
	limit: usize,
	name: Option<&'a str>,
	user_id: Option<&'a str>,
	admins: Option<bool>,
	locked: bool,
	deactivated: DeactivatedFilter,
	dir: Direction,
}

/// # `GET /_synapse/admin/v2/users`
pub(crate) async fn admin_list_users_v2_route(
	State(services): State<crate::State>,
	body: Ruma<v2::Request>,
) -> Result<v2::Response> {
	require_admin(&services, body.sender_user()).await?;

	let deactivated = match body.deactivated {
		| true => DeactivatedFilter::Include,
		| false => DeactivatedFilter::Exclude,
	};

	let params = ListParams {
		from: usize_from_ruma(body.from),
		limit: body.limit.map_or(100, usize_from_ruma),
		name: body.name.as_deref(),
		user_id: body.user_id.as_deref(),
		admins: body.admins,
		locked: body.locked,
		deactivated,
		dir: body.dir.unwrap_or(Direction::Forward),
	};

	let (users, next_token, total) = list_users(services, &params).await;

	Ok(v2::Response { users, next_token, total })
}

/// # `GET /_synapse/admin/v3/users`
pub(crate) async fn admin_list_users_v3_route(
	State(services): State<crate::State>,
	body: Ruma<v3::Request>,
) -> Result<v3::Response> {
	require_admin(&services, body.sender_user()).await?;

	let deactivated = match body.deactivated {
		| None => DeactivatedFilter::Any,
		| Some(true) => DeactivatedFilter::Only,
		| Some(false) => DeactivatedFilter::Exclude,
	};

	let params = ListParams {
		from: usize_from_ruma(body.from),
		limit: body.limit.map_or(100, usize_from_ruma),
		name: body.name.as_deref(),
		user_id: body.user_id.as_deref(),
		admins: body.admins,
		locked: body.locked,
		deactivated,
		dir: body.dir.unwrap_or(Direction::Forward),
	};

	let (users, next_token, total) = list_users(services, &params).await;

	Ok(v3::Response { users, next_token, total })
}

/// Returns the filtered, name-ordered and paginated user page, the `next_token`
/// (present only while the page does not reach the end of the filtered set) and
/// the filtered total. `order_by` beyond `name` is not backed by stored fields,
/// so the name ordering (reversed for `dir=b`) is the only one applied.
async fn list_users(
	services: crate::State,
	params: &ListParams<'_>,
) -> (Vec<UserMinorDetails>, Option<String>, UInt) {
	let mut names: Vec<String> = services
		.users
		.stream()
		.map(ToString::to_string)
		.collect()
		.await;

	names.sort_unstable();

	if matches!(params.dir, Direction::Backward) {
		names.reverse();
	}

	let matched: Vec<UserMinorDetails> = names
		.iter()
		.map(String::as_str)
		.stream()
		.wide_filter_map(async |name| user_minor_details(services, name, params).await)
		.collect()
		.await;

	let matched_count = matched.len();
	let total = ruma_from_usize(matched_count);

	let page: Vec<UserMinorDetails> = matched
		.into_iter()
		.skip(params.from)
		.take(params.limit)
		.collect();

	let end = params.from.saturating_add(page.len());
	let next_token = (end < matched_count).then(|| end.to_string());

	(page, next_token, total)
}

/// Applies the substring, admin, locked and deactivated filters to one user and
/// builds its `UserMinorDetails`, or returns `None` when the user is filtered
/// out.
async fn user_minor_details(
	services: crate::State,
	name: &str,
	params: &ListParams<'_>,
) -> Option<UserMinorDetails> {
	let user_id = UserId::parse(name).ok()?;

	let displayname = services.profile.displayname(&user_id).await.ok();

	if let Some(needle) = params.user_id.filter(|_| params.name.is_none())
		&& !name.contains(needle)
	{
		return None;
	}

	if let Some(needle) = params.name {
		let in_localpart = user_id.localpart().contains(needle);
		let in_displayname = displayname
			.as_deref()
			.is_some_and(|display| display.contains(needle));

		if !in_localpart && !in_displayname {
			return None;
		}
	}

	let admin = services.admin.user_is_admin(&user_id).await;
	if let Some(want_admin) = params.admins
		&& want_admin != admin
	{
		return None;
	}

	let locked = services.users.is_locked(&user_id).await;
	if locked && !params.locked {
		return None;
	}

	let deactivated = services
		.users
		.is_deactivated(&user_id)
		.await
		.unwrap_or(false);

	let keep = match params.deactivated {
		| DeactivatedFilter::Any | DeactivatedFilter::Include => true,
		| DeactivatedFilter::Only => deactivated,
		| DeactivatedFilter::Exclude => !deactivated,
	};

	if !keep {
		return None;
	}

	let avatar_url = services
		.profile
		.avatar_url(&user_id)
		.await
		.ok()
		.map(|url| url.to_string());

	let erased = services.users.is_erased(&user_id).await;

	let last_seen_ts = services
		.users
		.all_devices_metadata(&user_id)
		.ready_fold(None, |max, device| max.max(device.last_seen_ts))
		.await;

	Some(UserMinorDetails {
		displayname,
		avatar_url,
		admin,
		deactivated,
		locked,
		erased,
		last_seen_ts,
		// tuwunel has no creation timestamp; emit a 0 sentinel (strict clients reject null).
		creation_ts: Some(MilliSecondsSinceUnixEpoch(UInt::from(0_u32))),
		..UserMinorDetails::new(name.to_owned())
	})
}
