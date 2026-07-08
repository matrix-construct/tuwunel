//! Synapse admin API: user endpoints.

mod account_data;
mod allow_cross_signing_replacement;
mod create_or_modify;
mod deactivate_account;
mod get_details;
mod is_user_admin;
mod list_joined_rooms;
mod list_users;
mod lookup_threepid;
mod memberships;
mod pushers;
mod reset_password;
mod suspend;
mod username_available;
mod whois;

use futures::StreamExt;
use ruma::UserId;
use synapse_admin_api::users::UserDetails;
use tuwunel_core::utils::stream::ReadyExt;

pub(crate) use self::{
	account_data::admin_account_data_route,
	allow_cross_signing_replacement::admin_allow_cross_signing_replacement_route,
	create_or_modify::admin_create_or_modify_route,
	deactivate_account::admin_deactivate_account_route,
	get_details::admin_get_details_route,
	is_user_admin::admin_is_user_admin_route,
	list_joined_rooms::admin_list_joined_rooms_route,
	list_users::{admin_list_users_v2_route, admin_list_users_v3_route},
	lookup_threepid::admin_lookup_threepid_route,
	memberships::admin_memberships_route,
	pushers::admin_pushers_route,
	reset_password::admin_reset_password_route,
	suspend::admin_suspend_route,
	username_available::admin_username_available_route,
	whois::admin_whois_route,
};

/// Assembles the Synapse `UserDetails` for a local user from the fields tuwunel
/// persists. Fields tuwunel never stores (guest, shadow-banned, external IDs,
/// user type, appservice, consent) are left at their absent defaults.
async fn user_details(services: crate::State, user_id: &UserId) -> UserDetails {
	let displayname = services.profile.displayname(user_id).await.ok();

	let avatar_url = services
		.profile
		.avatar_url(user_id)
		.await
		.ok()
		.map(|url| url.to_string());

	let admin = services.admin.user_is_admin(user_id).await;
	let deactivated = services
		.users
		.is_deactivated(user_id)
		.await
		.unwrap_or(true);

	let locked = services.users.is_locked(user_id).await;
	let suspended = services.users.is_suspended(user_id).await;
	let erased = services.users.is_erased(user_id).await;

	let threepids = services
		.threepid
		.get_bindings(user_id)
		.collect()
		.await;

	let last_seen_ts = services
		.users
		.all_devices_metadata(user_id)
		.ready_fold(None, |max, device| max.max(device.last_seen_ts))
		.await;

	UserDetails {
		displayname,
		avatar_url,
		admin,
		deactivated,
		locked,
		suspended,
		erased,
		threepids,
		last_seen_ts,
		..UserDetails::new(user_id.to_string())
	}
}
