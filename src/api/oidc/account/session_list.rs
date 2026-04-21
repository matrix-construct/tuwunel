use std::cmp;

use const_str::format as const_format;
use futures::StreamExt;
use ruma::{MilliSecondsSinceUnixEpoch, UserId};
use tuwunel_core::{Result, utils::html::escape as html_escape};
use tuwunel_service::Services;

use super::{ACCOUNT_HEAD, ACCOUNT_JS_INCLUDE, ts_cell, url_encode};

pub(super) async fn sessions_list_html(services: &Services, user_id: &UserId) -> Result<String> {
	let mut devices: Vec<_> = services
		.users
		.all_devices_metadata(user_id)
		.collect()
		.await;

	// Newest sessions first (highest last_seen_ts at top, None treated as oldest)
	devices.sort_by_key(|b| cmp::Reverse(b.last_seen_ts));

	let mut rows = Vec::new();
	for device in &devices {
		let device_display_name = device
			.display_name
			.as_deref()
			.unwrap_or("Unknown device");

		let name = html_escape(device_display_name);
		let id_enc = url_encode(device.device_id.as_str());
		let id = html_escape(device.device_id.as_str());
		let ip = html_escape(device.last_seen_ip.as_deref().unwrap_or("—"));
		let ts_cell = device
			.last_seen_ts
			.as_ref()
			.map(MilliSecondsSinceUnixEpoch::as_secs)
			.map(u64::from)
			.map(ts_cell)
			.unwrap_or_default();

		rows.push(format!(
			r#"
			<tr>
				<td>{name}</td>
				<td><code>{id}</code></td>
				<td>{ip}</td>
				<td>{ts_cell}</td>
				<td class="center">
					<a href="/_tuwunel/oidc/account?action=org.matrix.session_view&device_id={id_enc}">
						View
					</a>
					<span class="sep"> | </span>
					<a
						href="/_tuwunel/oidc/account?action=org.matrix.session_end&device_id={id_enc}"
						class="err"
					>
						Sign out
					</a>
				</td>
			</tr>"#
		));
	}

	Ok(PAGE_HTML
		.replace("{uid}", &html_escape(user_id.as_str()))
		.replace("{dlen}", &devices.len().to_string())
		.replace("{rows}", &rows.join("")))
}

static PAGE_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Active Sessions</title>
	</head>
	<body class="wide">
		<h1>Active Sessions</h1>
		<p>
			Signed in as <strong>{{uid}}</strong>. {{dlen}} active session(s).
		</p>
		<table>
			<tr>
				<th>Name</th>
				<th>Device ID</th>
				<th>Last seen IP</th>
				<th>Last seen</th>
				<th class="center">Actions</th>
			</tr>
			{{rows}}
		</table>
		<div class="nav">
			<a href="/_tuwunel/oidc/account?action=org.matrix.profile">View Profile</a>
		</div>
		{ACCOUNT_JS_INCLUDE}
	</body>
</html>"#
);
