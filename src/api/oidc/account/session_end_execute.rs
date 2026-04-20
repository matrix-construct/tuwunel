use const_str::format as const_format;
use ruma::{OwnedDeviceId, UserId};
use tuwunel_core::{Err, Result, info, utils::html::escape as html_escape};
use tuwunel_service::Services;

use super::ACCOUNT_HEAD;

/// Executes the actual session deletion. Called only from the POST handler.
pub(super) async fn session_end_execute_html(
	services: &Services,
	user_id: &UserId,
	device_id: &str,
) -> Result<String> {
	if device_id.is_empty() {
		return Err!(Request(InvalidParam("device_id is required")));
	}

	let device_id_owned: OwnedDeviceId = device_id.into();
	if !services
		.users
		.device_exists(user_id, &device_id_owned)
		.await
	{
		return Err!(Request(NotFound("Session not found")));
	}

	services
		.users
		.remove_device(user_id, &device_id_owned)
		.await;

	info!(?user_id, ?device_id_owned, "Session signed out via account management page");

	Ok(PAGE_HTML
		.replace("{{did}}", &html_escape(device_id_owned.as_str()))
		.replace("{{uid}}", &html_escape(user_id.as_str())))
}

static PAGE_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Session Signed Out</title>
	</head>
	<body>
		<h1 class="ok">Session Signed Out</h1>
		<p>
			Session <code>{{did}}</code> for <strong>{{uid}}</strong> has been signed out.
		</p>
		<div class="nav">
			<a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">
				Back to sessions
			</a>
		</div>
	</body>
</html>"#
);
