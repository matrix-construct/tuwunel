use ruma::profile::ProfileFieldValue;
use serde_json::Value;
use tuwunel_core::{Result, err};

use super::PropagateTo;
use crate::{admin_command, utils::parse_active_local_user_id};

#[admin_command]
pub(super) async fn set_profile_key(
	&self,
	user_id: String,
	key: String,
	value: Vec<String>,
	clear: bool,
	propagate_to: Option<PropagateTo>,
) -> Result {
	let user_id = parse_active_local_user_id(self.services, &user_id).await?;

	let propagation = propagate_to.map(Into::into);

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
		.set_profile_keys(&user_id, &[profile_value], propagation)
		.await?;

	if clear {
		write!(self, "Cleared profile key {key:?} for {user_id}").await
	} else {
		write!(self, "Set profile key {key:?} for {user_id}").await
	}
}
