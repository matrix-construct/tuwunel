use ruma::{OwnedDeviceId, UserId};
use tuwunel_core::{Err, Result, err, utils::html::escape as html_escape};
use tuwunel_service::Services;

use super::{ACCOUNT_HEAD, ACCOUNT_JS_INCLUDE, ts_cell, url_encode};

pub(super) async fn session_view_html(
	services: &Services,
	user_id: &UserId,
	device_id: &str,
	login_token: &str,
) -> Result<String> {
	if device_id.is_empty() {
		return Err!(Request(InvalidParam("device_id is required")));
	}

	let device_id_owned: OwnedDeviceId = device_id.into();
	let device = services
		.users
		.get_device_metadata(user_id, &device_id_owned)
		.await
		.map_err(|_| err!(Request(NotFound("Session not found"))))?;

	let device_display_name = device
		.display_name
		.as_deref()
		.unwrap_or("Unknown device");

	let name = html_escape(device_display_name);
	let tok = html_escape(login_token);
	let ip = html_escape(device.last_seen_ip.as_deref().unwrap_or("—"));
	let id = html_escape(device.device_id.as_str());
	let id_enc = url_encode(device.device_id.as_str());

	let ts_cell = device
		.last_seen_ts
		.map(|t| u64::from(t.as_secs()))
		.map(ts_cell)
		.unwrap_or_default();

	// Link directly to account_callback (skips SSO) using the peeked login_token
	// so the user doesn't have to re-authenticate just to sign out a session.
	Ok(PAGE_HTML
		.replace("{{ACCOUNT_JS_INCLUDE}}", ACCOUNT_JS_INCLUDE)
		.replace("{{ACCOUNT_HEAD}}", ACCOUNT_HEAD)
		.replace("{{name}}", &name)
		.replace("{{tok}}", &tok)
		.replace("{{ip}}", &ip)
		.replace("{{id}}", &id)
		.replace("{{id_enc}}", &id_enc)
		.replace("{{ts_cell}}", &ts_cell)
		.replace("{{uid}}", &html_escape(user_id.as_str())))
}

static PAGE_HTML: &str = r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{{ACCOUNT_HEAD}}
		<title>Session: {{name}}</title>
	</head>
	<body>
		<h1>Session Details</h1>
		<p>
			Signed in as <strong>{{uid}}}</strong>.
		</p>
		<dl>
			<dt>Name</dt><dd>{{name}}</dd>
			<dt>Device ID</dt><dd><code>{{id}}</code></dd>
			<dt>Last seen IP</dt><dd>{{ip}}</dd>
			<dt>Last seen</dt><dd>{{ts_cell}}</dd>
		</dl>
		<div class="actions">
			<a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">
				Back to sessions
			</a>
			<a
				href="/_tuwunel/oidc/account_callback?action=org.matrix.session_end&device_id={{id_enc}}&loginToken={{tok}}"
				class="err"
			>
				Sign out this session
			</a>
		</div>
		{{ACCOUNT_JS_INCLUDE}}
	</body>
</html>
"#;
