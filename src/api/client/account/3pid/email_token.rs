use std::{net::IpAddr, time::Duration};

use ruma::{OwnedSessionId, thirdparty::Medium};
use tuwunel_core::{Result, err, utils::html::escape as html_escape};
use tuwunel_service::Services;
use url::form_urlencoded;

/// Lifetime of a pending email verification before it self-reaps.
const PENDING_TTL: Duration = Duration::from_hours(1);

/// Shared requestToken spine for the email medium: throttle both axes, open or
/// reuse a pending verification, and send the magic-link message when a token
/// is freshly minted. The caller supplies the already-canonicalized address and
/// performs its own directional in-use / not-found check beforehand.
pub(super) async fn send_email_token(
	services: &Services,
	client: IpAddr,
	client_secret: &str,
	email_canon: &str,
	send_attempt: u64,
) -> Result<OwnedSessionId> {
	services.sendmail.check_address(email_canon)?;
	services.threepid.check_ip_rate_limit(client)?;
	services
		.threepid
		.check_address_rate_limit(email_canon)?;

	let outcome = services
		.threepid
		.create_or_reuse_pending(
			client_secret,
			Medium::Email,
			email_canon,
			send_attempt,
			PENDING_TTL,
		)
		.await?;

	if let Some(token) = outcome.freshly_minted_token {
		let link = validate_link(services, &outcome.sid, client_secret, &token)?;
		let body = verification_html(&link);

		services
			.sendmail
			.send_to(email_canon, "Verify your email address", body)
			.await?;
	}

	outcome
		.sid
		.parse()
		.map_err(|_| err!("Generated an invalid session id"))
}

fn validate_link(
	services: &Services,
	sid: &str,
	client_secret: &str,
	token: &str,
) -> Result<String> {
	let base = services
		.config
		.well_known
		.client
		.as_ref()
		.map(ToString::to_string)
		.ok_or_else(|| {
			err!(Config(
				"well_known.client",
				"A public client base URL must be set to send email"
			))
		})?;

	let base = base.trim_end_matches('/');
	let query = form_urlencoded::Serializer::new(String::new())
		.append_pair("sid", sid)
		.append_pair("client_secret", client_secret)
		.append_pair("token", token)
		.finish();

	Ok(format!("{base}/_tuwunel/3pid/email/validate?{query}"))
}

fn verification_html(link: &str) -> String {
	let link = html_escape(link);

	format!(
		"<!DOCTYPE html>
<html lang=\"en\">
  <body>
    <h1>Verify your email address</h1>
    <p>Open the link below to confirm this address.</p>
    <p><a href=\"{link}\">{link}</a></p>
    <p>If you did not request this, you can ignore this message.</p>
  </body>
</html>"
	)
}
