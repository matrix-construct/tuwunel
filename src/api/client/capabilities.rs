use std::future::ready;

use axum::extract::State;
#[expect(deprecated)]
use ruma::api::client::discovery::get_capabilities::v3::{
	SetAvatarUrlCapability, SetDisplayNameCapability,
};
use ruma::{
	UInt,
	api::client::discovery::get_capabilities::v3::{
		AccountModerationCapability, Capabilities, ChangePasswordCapability,
		DelayedEventsCapability, ForgetForcedUponLeaveCapability, GetLoginTokenCapability,
		ProfileFieldsCapability, Request, Response, RoomVersionsCapability,
		ThirdPartyIdChangesCapability,
	},
	profile::ProfileFieldName,
};
use serde_json::json;
use tuwunel_core::{Result, utils::BoolExt};
use tuwunel_service::{Services, delayed_events::Limits};

use crate::{Ruma, utils::may_set_profile_field};

/// # `GET /_matrix/client/v3/capabilities`
///
/// Get information on the supported feature set and other relevant capabilities
/// of this server.
pub(crate) async fn get_capabilities_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	// MSC4323: advertise admin moderation only to admins; absence implies
	// neither suspend nor lock is available to the caller.
	let account_moderation = services
		.admin
		.user_is_admin(body.sender_user())
		.await;

	let may_set = async |field| {
		may_set_profile_field(&services, &body, &field, || ready(account_moderation)).await
	};

	let set_displayname = may_set(ProfileFieldName::DisplayName).await;
	let set_avatar_url = may_set(ProfileFieldName::AvatarUrl).await;

	capabilities(&services, set_displayname, set_avatar_url, account_moderation)
		.map(Response::new)
}

#[expect(deprecated)]
fn capabilities(
	services: &Services,
	set_displayname: bool,
	set_avatar_url: bool,
	account_moderation: bool,
) -> Result<Capabilities> {
	let available = services
		.config
		.supported_room_versions()
		.collect();

	let default = services
		.server
		.config
		.rooms
		.default_version
		.clone();

	// Matrix 1.16 clients read the profile field policy from m.profile_fields.
	let disallowed: Vec<_> = [
		(set_displayname, ProfileFieldName::DisplayName),
		(set_avatar_url, ProfileFieldName::AvatarUrl),
	]
	.into_iter()
	.filter_map(|(allowed, field)| allowed.is_false().then_some(field))
	.collect();

	let disallowed = disallowed
		.is_empty()
		.is_false()
		.then_some(disallowed);

	let mut capabilities = Capabilities::default(); // non_exhaustive: no struct literal

	capabilities.room_versions = RoomVersionsCapability { available, default };

	// MSC3283: deprecated displayname/avatar capabilities for pre-1.16 clients.
	capabilities.set_displayname = SetDisplayNameCapability::new(set_displayname);
	capabilities.set_avatar_url = SetAvatarUrlCapability::new(set_avatar_url);

	// 3PID add/remove is available only when the email subsystem can send.
	capabilities.thirdparty_id_changes =
		ThirdPartyIdChangesCapability { enabled: services.sendmail.is_enabled() };

	let enabled = services
		.server
		.config
		.authentication
		.login
		.via_existing_session;

	capabilities.get_login_token = GetLoginTokenCapability { enabled };

	capabilities.profile_fields = ProfileFieldsCapability {
		disallowed,
		..ProfileFieldsCapability::new(true)
	}
	.into();

	let enabled = services
		.server
		.config
		.authentication
		.login
		.with_password;

	capabilities.change_password = ChangePasswordCapability { enabled };

	capabilities.forget_forced_upon_leave =
		ForgetForcedUponLeaveCapability::new(services.config.rooms.forget_forced_upon_leave);

	capabilities.set(
		"org.matrix.msc4267.forget_forced_upon_leave",
		json!({"enabled": services.config.rooms.forget_forced_upon_leave}),
	)?;

	// MSC4452: stable and unstable names share the media service gate.
	set_preview_url(&mut capabilities, services.media.url_preview_enabled())?;

	// MSC3664: absent rather than present-and-disabled, since a client testing
	// for the key rather than its value would otherwise read support.
	if services.config.push.msc3664_related_event_match {
		capabilities.set("im.nheko.msc3664.related_event_match", json!({"enabled": true}))?;
	}

	if account_moderation {
		capabilities.account_moderation = AccountModerationCapability::new(true, true);
	}

	// MSC4140: absent while disabled; current clients skip a delayed leave
	// without one of the two names.
	let delayed_events = services
		.delayed_events
		.limits()
		.map(|limits| delayed_events_capability(DelayedEventsCapability::default(), limits));

	capabilities.delayed_events = delayed_events;
	capabilities.unstable_delayed_events = delayed_events;

	Ok(capabilities)
}

fn set_preview_url(capabilities: &mut Capabilities, enabled: bool) -> Result {
	let value = json!({"enabled": enabled});

	capabilities.set("m.preview_url", value.clone())?;
	capabilities.set("io.element.msc4452.preview_url", value)?;
	Ok(())
}

// non_exhaustive: no struct literal, so the limits fill a default in place
fn delayed_events_capability(
	mut capability: DelayedEventsCapability,
	limits: Limits,
) -> DelayedEventsCapability {
	capability.max_delay_ms = Some(UInt::new_saturating(limits.max_delay_ms));
	capability.max_scheduled = u64::try_from(limits.max_scheduled)
		.ok()
		.map(UInt::new_saturating);

	capability
}

#[cfg(test)]
mod tests {
	use super::{Capabilities, json, set_preview_url};

	#[test]
	fn preview_url_capability_names_agree() {
		for enabled in [false, true] {
			let mut capabilities = Capabilities::default();

			set_preview_url(&mut capabilities, enabled).expect("custom capabilities");
			let stable = capabilities
				.get("m.preview_url")
				.expect("stable capability");

			assert_eq!(stable.as_ref(), &json!({"enabled": enabled}));
			assert_eq!(capabilities.get("io.element.msc4452.preview_url"), Some(stable));
		}
	}
}
