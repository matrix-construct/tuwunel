use std::fmt::Write;

use const_str::format as const_format;
use futures::StreamExt;
use ruma::{OwnedDeviceId, UserId};
use tuwunel_core::{
	Result, info,
	utils::{
		html::escape as html_escape,
		stream::{IterStream, automatic_width},
	},
};
use tuwunel_service::Services;

use super::{ACCOUNT_HEAD, consume_login_token, selected_sessions, session_items};

/// Signs out the selected sessions.
///
/// Selected sessions already gone are skipped, and a selection with none left
/// fails without signing anything out or spending the login token, which is
/// consumed before the first removal. Each removal invalidates that session's
/// access token at once, and removals run concurrently in no particular order.
pub(super) async fn session_end_execute_html(
	services: &Services,
	user_id: &UserId,
	device_ids: &[OwnedDeviceId],
	login_token: &str,
) -> Result<String> {
	let sessions = selected_sessions(services, user_id, device_ids).await?;

	consume_login_token(services, Some(login_token)).await?;

	sessions
		.iter()
		.copied()
		.stream()
		.for_each_concurrent(automatic_width(), |device_id| {
			services.users.remove_device(user_id, device_id)
		})
		.await;

	info!(?user_id, ?sessions, "Sessions signed out via account management page");

	let items = session_items(&sessions, |out, did| write!(out, "<li><code>{did}</code></li>"));

	// Sessions last, so placeholder text in a device ID stays literal.
	let page = PAGE_HTML
		.replace("{uid}", &html_escape(user_id.as_str()))
		.replace("{count}", &sessions.len().to_string())
		.replace("{sessions}", &items);

	Ok(page)
}

static PAGE_HTML: &str = const_format!(
	r#"
<!DOCTYPE html>
<html lang="en">
	<head>
		{ACCOUNT_HEAD}
		<title>Sessions Signed Out</title>
	</head>
	<body>
		<h1 class="ok">Sessions Signed Out</h1>
		<p>
			Signed out {{count}} session(s) for <strong>{{uid}}</strong>.
		</p>
		<ul>{{sessions}}
		</ul>
		<div class="nav">
			<a href="/_tuwunel/oidc/account?action=org.matrix.sessions_list">
				Back to sessions
			</a>
		</div>
	</body>
</html>"#
);
