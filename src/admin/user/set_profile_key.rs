use ruma::{OwnedUserId, profile::ProfileFieldValue};
use serde_json::Value;
use tuwunel_core::{Err, Result, err};
use tuwunel_service::{Services, profile::Propagation};

use super::PropagateTo;
use crate::{
	admin_command,
	utils::{check_known_remote_user, parse_active_local_user_id, parse_user_id},
};

#[admin_command]
pub(super) async fn set_profile_key(
	&self,
	user_id: String,
	key: String,
	value: Vec<String>,
	clear: bool,
	propagate_to: Option<PropagateTo>,
) -> Result {
	let user_id = profile_owner(self.services, &user_id, clear, propagate_to.is_some()).await?;

	let propagation = propagate_to
		.map(Into::into)
		.unwrap_or(Propagation::All);

	let profile_value = if clear {
		(key.as_str().into(), None)
	} else {
		let value = value.join(" ");

		let value = serde_json::from_str(&value).unwrap_or(Value::String(value));

		let profile_value = ProfileFieldValue::new(&key, value)
			.map_err(|e| err!("Invalid value for profile key {key:?}: {e}"))?;

		(profile_value.field_name(), Some(profile_value.value().into_owned()))
	};

	self.services
		.profile
		.set_profile_keys(&user_id, &[profile_value], Some(propagation))
		.await?;

	if clear {
		write!(self, "Cleared profile key {key:?} for {user_id}").await
	} else {
		write!(self, "Set profile key {key:?} for {user_id}").await
	}
}

/// Resolves the command's target: an active local user, or with `--clear` and
/// no propagation a remote user this server has cached.
///
/// Locality is judged on the lowercased form, as `parse_active_local_user_id`
/// does, so a mixed-case spelling of a local user never reaches the remote
/// branch.
async fn profile_owner(
	services: &Services,
	user_id: &str,
	clear: bool,
	propagate: bool,
) -> Result<OwnedUserId> {
	let local = parse_user_id(services, user_id)
		.is_ok_and(|user_id| services.globals.user_is_local(&user_id));

	let remote = OwnedUserId::parse(user_id)
		.ok()
		.filter(|_| !local);

	match remote {
		| None => parse_active_local_user_id(services, user_id).await,
		| Some(_) if !clear => Err!("A remote user's profile key can only be cleared."),
		| Some(_) if propagate => Err!("Propagation applies only to a local user's profile."),
		| Some(user_id) => check_known_remote_user(services, &user_id)
			.await
			.map(|()| user_id),
	}
}
