mod get_nonce;
mod is_user_locked;
mod is_user_suspended;
mod lock_user;
pub(crate) mod mas;
mod register;
mod suspend_user;

pub(crate) mod devices;
pub(crate) mod federation;
pub(crate) mod media;
pub(crate) mod misc;
pub(crate) mod rooms;
pub(crate) mod tokens;
pub(crate) mod users;

use futures::future::join3;
use ruma::UserId;
use tuwunel_core::{Config, Err, Result, err};

pub(crate) use self::{
	get_nonce::admin_register_nonce_route, is_user_locked::is_user_locked_route,
	is_user_suspended::is_user_suspended_route, lock_user::lock_user_route,
	register::admin_register_route, suspend_user::suspend_user_route,
};

/// MSC4323: authorization is checked before account lookups
/// (anti-enumeration) per spec.
async fn authorize(services: &crate::State, caller: &UserId, target: &UserId) -> Result {
	if caller == target {
		return Err!(Request(Forbidden("You cannot suspend or lock your own account")));
	}

	if !services.globals.user_is_local(target) {
		return Err!(Request(InvalidParam("User is not local to this server")));
	}

	let (caller_admin, target_active, target_admin) = join3(
		services.admin.user_is_admin(caller),
		services.users.is_active(target),
		services.admin.user_is_admin(target),
	)
	.await;

	if !caller_admin {
		return Err!(Request(Forbidden("Only server administrators can use this endpoint")));
	}

	if !target_active {
		return Err!(Request(NotFound("Unknown user")));
	}

	if target_admin {
		return Err!(Request(Forbidden(
			"You cannot suspend or lock another server administrator"
		)));
	}

	Ok(())
}

/// Assert the caller is a server administrator. Generic Synapse admin
/// endpoints use this plain check, not the MSC4323 anti-enumeration
/// `authorize()` guard whose self-target and admin-target ordering does not
/// fit them.
#[expect(dead_code)]
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

/// True when Matrix Authentication Service delegation is active, in which case
/// the Synapse-mirrored admin routes MAS owns (user admin, login-as,
/// reset_password, registration tokens) are left unregistered and answer 404,
/// mirroring Synapse's de-registration of those servlets.
pub(crate) fn mas_active(config: &Config) -> bool {
	config
		.mas_secret
		.as_deref()
		.is_some_and(|secret| !secret.is_empty())
}
