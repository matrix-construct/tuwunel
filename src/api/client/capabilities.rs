use std::collections::BTreeMap;

use axum::extract::State;
#[expect(deprecated)]
use ruma::api::client::discovery::get_capabilities::v3::{
	SetAvatarUrlCapability, SetDisplayNameCapability,
};
use ruma::{
	RoomVersionId,
	api::client::discovery::{
		get_capabilities,
		get_capabilities::v3::{
			AccountModerationCapability, Capabilities, ChangePasswordCapability,
			ForgetForcedUponLeaveCapability, GetLoginTokenCapability, ProfileFieldsCapability,
			RoomVersionStability, RoomVersionsCapability, ThirdPartyIdChangesCapability,
		},
	},
};
use serde_json::json;
use tuwunel_core::Result;

use crate::Ruma;

/// # `GET /_matrix/client/v3/capabilities`
///
/// Get information on the supported feature set and other relevant capabilities
/// of this server.
#[expect(deprecated)]
pub(crate) async fn get_capabilities_route(
	State(services): State<crate::State>,
	body: Ruma<get_capabilities::v3::Request>,
) -> Result<get_capabilities::v3::Response> {
	let available: BTreeMap<RoomVersionId, RoomVersionStability> = services
		.config
		.supported_room_versions()
		.collect();

	let mut capabilities = Capabilities::default();
	capabilities.room_versions = RoomVersionsCapability {
		available,
		default: services
			.server
			.config
			.default_room_version
			.clone(),
	};

	// MSC3283: deprecated displayname/avatar capabilities for pre-1.16 clients.
	capabilities.set_displayname = SetDisplayNameCapability::new(true);
	capabilities.set_avatar_url = SetAvatarUrlCapability::new(true);

	// 3PID add/remove is available only when the email subsystem can send.
	capabilities.thirdparty_id_changes =
		ThirdPartyIdChangesCapability { enabled: services.sendmail.is_enabled() };

	capabilities.get_login_token = GetLoginTokenCapability {
		enabled: services.server.config.login_via_existing_session,
	};

	capabilities.profile_fields = ProfileFieldsCapability::new(true).into();

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
	let preview_url_enabled = !services
		.config
		.url_preview_domain_contains_allowlist
		.is_empty()
		|| !services
			.config
			.url_preview_domain_explicit_allowlist
			.is_empty()
		|| !services
			.config
			.url_preview_url_contains_allowlist
			.is_empty();

	capabilities
		.set("io.element.msc4452.preview_url", json!({"enabled": preview_url_enabled}))?;

	// MSC4323: advertise admin moderation only to admins; absence implies
	// neither suspend nor lock is available to the caller.
	if services
		.admin
		.user_is_admin(body.sender_user())
		.await
	{
		capabilities.account_moderation = AccountModerationCapability::new(true, true);
	}

	Ok(get_capabilities::v3::Response { capabilities })
}
