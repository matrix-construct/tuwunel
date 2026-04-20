use const_str::format as const_format;
use ruma::UserId;
use tuwunel_core::utils::html::escape as html_escape;

use super::{ACCOUNT_HEAD, url_encode};

/// Shows a POST confirmation form. The `login_token` is the original SSO-issued
/// token, peeked (not consumed) by the GET handler and embedded here as the
/// CSRF/auth token. It is consumed when the user submits this form.
pub(super) fn session_end_confirm_html(
	user_id: &UserId,
	device_id: &str,
	login_token: &str,
) -> String {
	let uid = html_escape(user_id.as_str());
	let did = html_escape(device_id);
	let tok = html_escape(login_token);

	// url_encode for use in the Cancel href query parameter.
	let did_enc = url_encode(device_id);
	let tok_enc = url_encode(login_token);

	PAGE_HTML
		.replace("{{uid}}", &uid)
		.replace("{{did}}", &did)
		.replace("{{tok}}", &tok)
		.replace("{{did_enc}}", &did_enc)
		.replace("{{tok_enc}}", &tok_enc)
}

static PAGE_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Sign Out Session</title>
	</head>
	<body>
		<h1>Sign Out Session</h1>
		<p>
			Signed in as <strong>{{uid}}</strong>.
		</p>
		<p class="warn">
			Sign out session <code>{{did}}</code>?
			This will immediately invalidate its access token.
		</p>
		<form method="POST" action="/_tuwunel/oidc/account_callback">
			<input type="hidden" name="action" value="org.matrix.session_end">
			<input type="hidden" name="device_id" value="{{did}}">
			<input type="hidden" name="loginToken" value="{{tok}}">
			<button type="submit" class="danger">Sign out</button>
			<a
				class="cancel"
				href="/_tuwunel/oidc/account_callback?action=org.matrix.session_view&device_id={{did_enc}}&loginToken={{tok_enc}}"
			>
				Cancel
			</a>
		</form>
	</body>
</html>"#
);
