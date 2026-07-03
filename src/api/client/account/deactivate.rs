use axum::extract::State;
use futures::FutureExt;
use ruma::api::client::account::{ThirdPartyIdRemovalStatus, deactivate};
use tuwunel_core::{Result, info};

use crate::{ClientIp, Ruma, router::auth_uiaa};

/// # `POST /_matrix/client/r0/account/deactivate`
///
/// Deactivate sender user account.
///
/// - Leaves all rooms and rejects all invitations
/// - Invalidates all access tokens
/// - Deletes all device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets all to-device events
/// - Triggers device list updates
/// - Removes ability to log in again
#[tracing::instrument(skip_all, fields(%client), name = "deactivate")]
pub(crate) async fn deactivate_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<deactivate::v3::Request>,
) -> Result<deactivate::v3::Response> {
	let ref sender_user = auth_uiaa(&services, &body).await?;

	services
		.deactivate
		.full_deactivate(sender_user, body.erase)
		.boxed()
		.await?;

	info!("User {sender_user} deactivated their account.");
	services
		.admin
		.notify(&format!("User {sender_user} deactivated their account."))
		.await;

	Ok(deactivate::v3::Response {
		id_server_unbind_result: ThirdPartyIdRemovalStatus::NoSupport,
	})
}
