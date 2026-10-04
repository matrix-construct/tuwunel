use std::future::ready;

use axum::extract::State;
#[expect(deprecated)]
use ruma::api::client::discovery::get_capabilities::v3::{
	SetAvatarUrlCapability, SetDisplayNameCapability,
};
use ruma::{
	api::client::discovery::get_capabilities::v3::{
		AccountModerationCapability, Capabilities, ChangePasswordCapability,
		ForgetForcedUponLeaveCapability, GetLoginTokenCapability, ProfileFieldsCapability,
		Request, Response, RoomVersionsCapability, ThirdPartyIdChangesCapability,
	},
	profile::ProfileFieldName,
};
use serde_json::json;
use tuwunel_core::{Result, utils::BoolExt};
use tuwunel_service::Services;

use crate::{Ruma, utils::may_set_displayname};

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

	let set_displayname =
		may_set_displayname(&services, &body, || ready(account_moderation)).await;

	capabilities(&services, set_displayname, account_moderation).map(Response::new)
}

#[expect(deprecated)]
fn capabilities(
	services: &Services,
	set_displayname: bool,
	account_moderation: bool,
) -> Result<Capabilities> {
	let available = services
		.config
		.supported_room_versions()
		.collect();

	let default = services
		.server
		.config
		.default_room_version
		.clone();

	// Matrix 1.16 clients read the display name policy from m.profile_fields.
	let disallowed = set_displayname
		.is_false()
		.then(|| vec![ProfileFieldName::DisplayName]);

	let mut capabilities = Capabilities::default(); // non_exhaustive: no struct literal

	capabilities.room_versions = RoomVersionsCapability { available, default };

	// MSC3283: deprecated displayname/avatar capabilities for pre-1.16 clients.
	capabilities.set_displayname = SetDisplayNameCapability::new(set_displayname);
	capabilities.set_avatar_url = SetAvatarUrlCapability::new(true);

	// 3PID add/remove is available only when the email subsystem can send.
	capabilities.thirdparty_id_changes =
		ThirdPartyIdChangesCapability { enabled: services.sendmail.is_enabled() };

	capabilities.get_login_token = GetLoginTokenCapability {
		enabled: services.server.config.login_via_existing_session,
	};

	capabilities.profile_fields = ProfileFieldsCapability {
		disallowed,
		..ProfileFieldsCapability::new(true)
	}
	.into();

	capabilities.change_password = ChangePasswordCapability {
		enabled: services.server.config.login_with_password,
	};

	capabilities.forget_forced_upon_leave =
		ForgetForcedUponLeaveCapability::new(services.config.forget_forced_upon_leave);

	capabilities.set(
		"org.matrix.msc4267.forget_forced_upon_leave",
		json!({"enabled": services.config.forget_forced_upon_leave}),
	)?;

	// MSC4452: enabled mirrors the per-URL gate; empty allowlists 403 every URL.
	capabilities.set(
		"io.element.msc4452.preview_url",
		json!({"enabled": preview_url_enabled(services)}),
	)?;

	// MSC3664: absent rather than present-and-disabled, since a client testing
	// for the key rather than its value would otherwise read support.
	if services.config.msc3664_related_event_match {
		capabilities.set("im.nheko.msc3664.related_event_match", json!({"enabled": true}))?;
	}

	if account_moderation {
		capabilities.account_moderation = AccountModerationCapability::new(true, true);
	}

	Ok(capabilities)
}

fn preview_url_enabled(services: &Services) -> bool {
	let config = &services.config;

	!config
		.url_preview_domain_contains_allowlist
		.is_empty()
		|| !config
			.url_preview_domain_explicit_allowlist
			.is_empty()
		|| !config
			.url_preview_url_contains_allowlist
			.is_empty()
}
