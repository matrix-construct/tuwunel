use const_str::format as const_format;
use ruma::UserId;
use tuwunel_core::{Result, info, utils::html::escape as html_escape};
use tuwunel_service::{Services, users::DeactivationReason};

use super::{ACCOUNT_HEAD, url_encode};

/// Shows a POST confirmation form for account deactivation. The `login_token`
/// is peeked (not consumed) by the GET handler and embedded here; submitting
/// the form consumes it, re-authenticating this destructive action.
pub(super) async fn account_deactivate_confirm_html(
	user_id: &UserId,
	login_token: &str,
) -> Result<String> {
	let uid = html_escape(user_id.as_str());
	let tok = html_escape(login_token);
	let tok_enc = url_encode(login_token);

	Ok(CONFIRM_HTML
		.replace("{uid}", &uid)
		.replace("{tok}", &tok)
		.replace("{tok_enc}", &tok_enc))
}

/// Executes the account deactivation, signing out every session and blocking
/// future logins. Called only from the POST handler once the token is consumed.
pub(super) async fn account_deactivate_execute_html(
	services: &Services,
	user_id: &UserId,
) -> Result<String> {
	services
		.users
		.deactivate_account(user_id, DeactivationReason::SelfService)
		.await?;

	info!(?user_id, "Account deactivated via account management page");

	Ok(EXECUTE_HTML.replace("{uid}", &html_escape(user_id.as_str())))
}

static CONFIRM_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Deactivate Account</title>
	</head>
	<body>
		<h1>Deactivate Account</h1>
		<p>
			Signed in as <strong>{{uid}}</strong>.
		</p>
		<p class="warn">
			Deactivate your account? This signs out all of your sessions and
			permanently prevents logging in again. This cannot be undone.
		</p>
		<form method="POST" action="/_tuwunel/oidc/account_callback">
			<input type="hidden" name="action" value="org.matrix.account_deactivate">
			<input type="hidden" name="loginToken" value="{{tok}}">
			<button type="submit" class="danger">Deactivate account</button>
			<a
				class="cancel"
				href="/_tuwunel/oidc/account_callback?action=org.matrix.sessions_list&loginToken={{tok_enc}}"
			>
				Cancel
			</a>
		</form>
	</body>
</html>"#
);

static EXECUTE_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Account Deactivated</title>
	</head>
	<body>
		<h1 class="ok">Account Deactivated</h1>
		<p>
			Account <strong>{{uid}}</strong> has been deactivated and all of its
			sessions signed out.
		</p>
	</body>
</html>"#
);
