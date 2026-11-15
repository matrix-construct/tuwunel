use std::cmp::Reverse;

use const_str::format as const_format;
use futures::StreamExt;
use itertools::Itertools;
use ruma::{MilliSecondsSinceUnixEpoch, UserId, api::client::device::Device};
use tuwunel_core::{Result, utils::html::escape as html_escape};
use tuwunel_service::Services;

use super::{ACCOUNT_HEAD, ACCOUNT_JS_INCLUDE, ts_cell, url_encode};

/// Lists the user's sessions, newest first, in a form to sign out a selection.
///
/// The `login_token` was peeked, not consumed, so the form carries it to the
/// confirmation page, which must be reached before the token expires.
pub(super) async fn sessions_list_html(
	services: &Services,
	user_id: &UserId,
	login_token: &str,
) -> Result<String> {
	// A session never seen sorts as the oldest.
	let devices = services
		.users
		.all_devices_metadata(user_id)
		.collect::<Vec<_>>()
		.await
		.into_iter()
		.sorted_by_key(|device| Reverse(device.last_seen_ts));

	let dlen = devices.len();
	let rows: String = devices
		.map(|device| session_row(&device))
		.collect();

	// Rows last, so placeholder text in a device name stays literal.
	let page = PAGE_HTML
		.replace("{uid}", &html_escape(user_id.as_str()))
		.replace("{dlen}", &dlen.to_string())
		.replace("{tok}", &html_escape(login_token))
		.replace("{rows}", &rows);

	Ok(page)
}

fn session_row(device: &Device) -> String {
	let display_name = device
		.display_name
		.as_deref()
		.unwrap_or("Unknown device");

	let name = html_escape(display_name);
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

	format!(
		r#"
			<tr>
				<td class="center">
					<input type="checkbox" name="device_id" value="{id}" aria-label="Select {name}">
				</td>
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
	)
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
		<form method="GET" action="/_tuwunel/oidc/account_callback">
			<input type="hidden" name="action" value="org.matrix.session_end">
			<input type="hidden" name="loginToken" value="{{tok}}">
			<table>
				<thead>
					<tr>
						<th class="center">Select</th>
						<th data-sort="text">Name</th>
						<th data-sort="text">Device ID</th>
						<th data-sort="text">Last seen IP</th>
						<th data-sort="time" aria-sort="descending">Last seen</th>
						<th class="center">Actions</th>
					</tr>
				</thead>
				<tbody>{{rows}}
				</tbody>
			</table>
			<div class="submit-row">
				<button type="submit" class="danger">Sign out selected</button>
			</div>
		</form>
		<div class="nav">
			<a href="/_tuwunel/oidc/account?action=org.matrix.profile">View Profile</a>
		</div>
		{ACCOUNT_JS_INCLUDE}
	</body>
</html>"#
);
