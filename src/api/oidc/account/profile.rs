use const_str::format as const_format;
use ruma::UserId;
use tuwunel_core::{Result, utils::html::escape as html_escape};
use tuwunel_service::Services;

use super::ACCOUNT_HEAD;

pub(super) async fn profile_html(
	services: &Services,
	user_id: &UserId,
	login_token: &str,
) -> Result<String> {
	let server = services.config.server_name.as_str();

	let displayname = services
		.users
		.displayname(user_id)
		.await
		.unwrap_or_default();

	let avatar_url = services
		.users
		.avatar_url(user_id)
		.await
		.ok()
		.as_ref()
		.map(ToString::to_string)
		.as_deref()
		.map(html_escape);

	let avatar_field = avatar_url
		.as_deref()
		.map(|avatar_url| {
			format!(
				r#"<p class="meta">
					Avatar: <code>{avatar_url}</code> (use your Matrix client to change)
				</p>"#
			)
		})
		.unwrap_or_default();

	Ok(PAGE_HTML
		.replace("{{server}}", &html_escape(server))
		.replace("{{uid}}", &html_escape(user_id.as_str()))
		.replace("{{tok}}", &html_escape(login_token))
		.replace("{{dn}}", &html_escape(&displayname))
		.replace("{{avatar_field}}", &avatar_field))
}

static PAGE_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Profile</title>
	</head>
	<body>
		<h1>Profile</h1>
		<p>
			Signed in as <strong>{{uid}}</strong> on <strong>{{server}}</strong>.
		</p>
		<form method="POST" action="/_tuwunel/oidc/account_callback">
			<input type="hidden" name="action" value="org.matrix.profile" />
			<input type="hidden" name="loginToken" value="{{tok}}" />
			<label for="dn">Display name</label>
			<input
				type="text"
				id="dn"
				name="displayname"
				value="{{dn}}"
				maxlength="255"
				autocomplete="name"
			/>
				{{avatar_field}}
			<p class="submit-row">
				<button type="submit">Save</button>
			</p>
		</form>
		<div class="nav">
			<a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">
				Back to sessions
			</a>
		</div>
	</body>
</html>
"#
);
