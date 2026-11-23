use std::borrow::Cow;

use const_str::format as const_format;
use ruma::UserId;
use tuwunel_core::{
	Result,
	utils::{html::escape_template_value as escape, result::NotFound},
	warn,
};
use tuwunel_service::Services;

use super::{ACCOUNT_HEAD, may_set_displayname};

pub(super) async fn profile_html(
	services: &Services,
	user_id: &UserId,
	login_token: &str,
) -> Result<String> {
	let server = services.config.server_name.as_str();

	let displayname = services
		.profile
		.displayname(user_id)
		.await
		.unwrap_or_default();

	let avatar_url = services
		.profile
		.avatar_url(user_id)
		.await
		.optional()
		.inspect_err(|error| warn!(%user_id, %error, "Unreadable avatar omitted."))
		.ok()
		.flatten();

	let avatar_field = avatar_url
		.as_ref()
		.map(|avatar_url| escape(avatar_url.as_str()))
		.map(|avatar_url| {
			format!(
				r#"<p class="meta">
					Avatar: <code>{avatar_url}</code> (use your Matrix client to change)
				</p>"#
			)
		})
		.unwrap_or_default();

	let page: Cow<'_, str> = if may_set_displayname(services, user_id).await {
		FORM_PAGE_HTML
			.replace("{tok}", &escape(login_token))
			.into()
	} else {
		READ_ONLY_PAGE_HTML.into()
	};

	let html = page
		.replace("{server}", &escape(server))
		.replace("{uid}", &escape(user_id.as_str()))
		.replace("{dn}", &escape(&displayname))
		.replace("{avatar_field}", &avatar_field);

	Ok(html)
}

static FORM_PAGE_HTML: &str = const_format!("{PAGE_TOP}{FORM}{PAGE_BOTTOM}");

static READ_ONLY_PAGE_HTML: &str = const_format!("{PAGE_TOP}{READ_ONLY}{PAGE_BOTTOM}");

static PAGE_TOP: &str = const_format!(
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
		</p>"#
);

static FORM: &str = r#"
		<form method="POST" action="/_tuwunel/oidc/account_callback">
			<input type="hidden" name="action" value="org.matrix.profile" />
			<input type="hidden" name="loginToken" value="{tok}" />
			<label for="dn">Display name</label>
			<input
				type="text"
				id="dn"
				name="displayname"
				value="{dn}"
				maxlength="255"
				autocomplete="name"
			/>
				{avatar_field}
			<p class="submit-row">
				<button type="submit">Save</button>
			</p>
		</form>"#;

static READ_ONLY: &str = r#"
		<p class="meta">
			Display name: <strong>{dn}</strong> (changing it is disabled on this server)
		</p>
		{avatar_field}"#;

static PAGE_BOTTOM: &str = r#"
		<div class="nav">
			<a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">
				Back to sessions
			</a>
		</div>
	</body>
</html>
"#;
