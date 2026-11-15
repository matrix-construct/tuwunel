use std::fmt::Write;

use const_str::format as const_format;
use ruma::{OwnedDeviceId, UserId};
use tuwunel_core::{Result, utils::html::escape as html_escape};
use tuwunel_service::Services;

use super::{ACCOUNT_HEAD, selected_sessions, session_items, url_encode};

/// Shows a POST confirmation form for signing out the selected sessions.
///
/// The `login_token` is the original SSO-issued token, peeked (not consumed)
/// by the GET handler and embedded here as the CSRF/auth token. It is consumed
/// when the user submits this form. Cancel returns to the session for one, and
/// to the list for several.
pub(super) async fn session_end_confirm_html(
	services: &Services,
	user_id: &UserId,
	device_ids: &[OwnedDeviceId],
	login_token: &str,
) -> Result<String> {
	let sessions = selected_sessions(services, user_id, device_ids).await?;
	let tok_enc = url_encode(login_token);
	let cancel = match sessions.as_slice() {
		| [device_id] => format!(
			"/_tuwunel/oidc/account_callback?action=org.matrix.session_view&device_id={}&\
			 loginToken={tok_enc}",
			url_encode(device_id.as_str())
		),
		| _ => format!(
			"/_tuwunel/oidc/account_callback?action=org.matrix.sessions_list&\
			 loginToken={tok_enc}"
		),
	};

	let items = session_items(&sessions, |out, did| {
		write!(
			out,
			r#"
				<li>
					<code>{did}</code>
					<input type="hidden" name="device_id" value="{did}">
				</li>"#
		)
	});

	// Sessions last, so placeholder text in a device ID stays literal.
	let page = PAGE_HTML
		.replace("{uid}", &html_escape(user_id.as_str()))
		.replace("{count}", &sessions.len().to_string())
		.replace("{tok}", &html_escape(login_token))
		.replace("{cancel}", &cancel)
		.replace("{sessions}", &items);

	Ok(page)
}

static PAGE_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Sign Out Sessions</title>
	</head>
	<body>
		<h1>Sign Out Sessions</h1>
		<p>
			Signed in as <strong>{{uid}}</strong>.
		</p>
		<p class="warn">
			Sign out {{count}} session(s)?
			This will immediately invalidate their access tokens.
		</p>
		<form method="POST" action="/_tuwunel/oidc/account_callback">
			<input type="hidden" name="action" value="org.matrix.session_end">
			<input type="hidden" name="loginToken" value="{{tok}}">
			<ul>{{sessions}}
			</ul>
			<button type="submit" class="danger">Sign out</button>
			<a class="cancel" href="{{cancel}}">
				Cancel
			</a>
		</form>
	</body>
</html>"#
);
