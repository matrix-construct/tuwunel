use const_str::format as const_format;
use ruma::UserId;
use tuwunel_core::{Result, utils::html::escape as html_escape};

use super::ACCOUNT_HEAD;

pub(super) async fn profile_saved_html(
	user_id: &UserId,
	displayname: Option<&str>,
) -> Result<String> {
	let uid = html_escape(user_id.as_str());
	let dn = html_escape(displayname.unwrap_or("(none)"));

	Ok(PAGE_HTML
		.replace("{uid}", &uid)
		.replace("{dn}", &dn))
}

static PAGE_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Profile Saved</title>
	</head>
	<body>
		<h1 class="ok">Profile Saved</h1>
		<p>
			Display name for <strong>{{uid}}</strong> updated to: <strong>{{dn}}</strong>.
		</p>
		<div class="nav">
			<a href="/_tuwunel/oidc/account?action=org.matrix.profile">Edit profile</a>
			<a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">Back to sessions</a>
		</div>
	</body>
</html>
"#
);
