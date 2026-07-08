use axum::extract::State;
use futures::StreamExt;
use ruma::{UserId, api::client::admin::get_user_info::v3 as get_user_info};
use tuwunel_core::{Err, Result};

use crate::{Ruma, RumaResponse, client::admin::require_admin};

pub(crate) async fn admin_whois_route(
	State(services): State<crate::State>,
	body: Ruma<get_user_info::Request>,
) -> Result<RumaResponse<get_user_info::Response>> {
	let target: &UserId = &body.user_id;
	if body.sender_user() != target {
		require_admin(&services, body.sender_user()).await?;
	}

	if !services.globals.user_is_local(target) {
		return Err!(Request(InvalidParam("Can only look up local users")));
	}

	let connections = services
		.users
		.all_devices_metadata(target)
		.map(|device| get_user_info::ConnectionInfo {
			ip: device.last_seen_ip.map(|ip| ip.to_string()),
			last_seen: device.last_seen_ts,
			user_agent: None,
		})
		.collect()
		.await;

	let sessions = vec![get_user_info::SessionInfo { connections }];
	let device = get_user_info::DeviceInfo { sessions };
	let devices = [(String::new(), device)].into();

	Ok(RumaResponse(get_user_info::Response {
		user_id: Some(body.user_id.clone()),
		devices,
	}))
}
