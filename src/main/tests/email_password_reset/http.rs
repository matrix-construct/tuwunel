use std::{borrow::Cow, net::TcpListener, time::Duration};

use futures::future::join;
use quoted_printable::{ParseMode, decode};
use reqwest::{Client, Url};
use serde_json::{Value, json};
use serde_urlencoded::from_str;
use tokio::{
	io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
	net::TcpListener as TokioTcpListener,
	time::{sleep, timeout},
};
use tuwunel_core::{
	Err, Result, err,
	ruma::{
		UserId,
		exports::serde::{Deserialize, Serialize},
	},
};

use super::Reset;

pub(super) struct PasswordTokenRequest<'a> {
	pub(super) client: &'a Client,
	pub(super) smtp_listener: TcpListener,
	pub(super) base: &'a str,
	pub(super) email: &'a str,
	pub(super) canonical_email: &'a str,
	pub(super) client_secret: &'a str,
}

#[derive(Deserialize)]
#[serde(crate = "tuwunel_core::ruma::exports::serde")]
struct ValidationQuery<'a> {
	#[serde(borrow)]
	sid: Cow<'a, str>,
	#[serde(borrow)]
	client_secret: Cow<'a, str>,
	#[serde(borrow)]
	token: Cow<'a, str>,
}

pub(super) struct AuthenticatedPasswordChange<'a> {
	pub(super) client: &'a Client,
	pub(super) base: &'a str,
	pub(super) user_id: &'a UserId,
	pub(super) access_token: &'a str,
	pub(super) old_password: &'a str,
	pub(super) new_password: &'a str,
	pub(super) session: &'a str,
}

#[derive(Serialize)]
#[serde(crate = "tuwunel_core::ruma::exports::serde")]
struct PasswordBody<'a> {
	auth: EmailIdentityAuth<'a>,
	new_password: &'a str,
	#[serde(skip_serializing_if = "Option::is_none")]
	logout_devices: Option<bool>,
	user_id: &'a str,
	email: &'a str,
	device_id: &'a str,
	identifier: UserIdentifier<'a>,
}

#[derive(Serialize)]
#[serde(crate = "tuwunel_core::ruma::exports::serde")]
struct EmailIdentityAuth<'a> {
	#[serde(rename = "type")]
	kind: &'static str,
	threepid_creds: ThreepidCredentials<'a>,
	session: &'a str,
}

#[derive(Serialize)]
#[serde(crate = "tuwunel_core::ruma::exports::serde")]
struct ThreepidCredentials<'a> {
	sid: &'a str,
	client_secret: &'a str,
}

#[derive(Serialize)]
#[serde(crate = "tuwunel_core::ruma::exports::serde")]
struct UserIdentifier<'a> {
	#[serde(rename = "type")]
	kind: &'static str,
	user: &'a str,
}

pub(super) async fn request_token_failure(
	client: &Client,
	base: &str,
	email: &str,
	client_secret: &str,
) -> Result<(u16, String)> {
	let body = json!({
		"client_secret": client_secret,
		"email": email,
		"send_attempt": 1,
	});

	let response = client
		.post(format!("{base}/_matrix/client/v3/account/password/email/requestToken"))
		.header("connection", "close")
		.json(&body)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
}

pub(super) async fn password_uiaa_session(
	client: &Client,
	base: &str,
	token: &str,
) -> Result<String> {
	let body = json!({
		"logout_devices": false,
		"new_password": "unused-b-session-password",
	});

	let response = client
		.post(format!("{base}/_matrix/client/v3/account/password"))
		.bearer_auth(token)
		.header("connection", "close")
		.json(&body)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.json::<Value>().await?;

	assert_eq!(status, 401, "user B UIAA challenge: {body}");

	body.get("session")
		.and_then(Value::as_str)
		.map(ToOwned::to_owned)
		.ok_or_else(|| err!("user B UIAA challenge omitted session"))
}

pub(super) async fn request_password_token(
	PasswordTokenRequest {
		client,
		smtp_listener,
		base,
		email,
		canonical_email,
		client_secret,
	}: PasswordTokenRequest<'_>,
) -> Result<(String, String)> {
	let body = json!({
		"client_secret": client_secret,
		"email": email,
		"send_attempt": 1,
	});

	let request = client
		.post(format!("{base}/_matrix/client/v3/account/password/email/requestToken"))
		.header("connection", "close")
		.json(&body)
		.send();

	let message = timeout(Duration::from_secs(10), capture_email(smtp_listener));
	let (response, message) = join(request, message).await;
	let response = response?;
	let message = message.map_err(|_| err!("SMTP capture timed out"))??;
	let status = response.status().as_u16();
	let response = response.text().await?;

	assert_eq!(status, 200, "password requestToken: {response}");
	assert!(
		message.contains(canonical_email),
		"verification email used a noncanonical address"
	);

	let response = serde_json::from_str::<Value>(&response)?;
	let sid = response
		.get("sid")
		.and_then(Value::as_str)
		.map(ToOwned::to_owned)
		.ok_or_else(|| err!("password requestToken response omitted sid"))?;

	let link = validation_link(&message)?;
	let prefix = format!("{base}/_tuwunel/3pid/email/validate?");

	assert!(
		link.as_str().starts_with(&prefix),
		"verification email used an unexpected link: {link}"
	);

	let ValidationQuery {
		sid: actual_sid,
		client_secret: actual_secret,
		token,
	} = from_str(link.query().unwrap_or_default())?;
	let token = token.into_owned();

	assert_eq!(actual_sid.as_ref(), sid.as_str());
	assert_eq!(actual_secret.as_ref(), client_secret);
	assert!(!token.is_empty(), "verification email carried an empty token");

	Ok((sid, token))
}

async fn capture_email(listener: TcpListener) -> Result<String> {
	listener.set_nonblocking(true)?;

	let listener = TokioTcpListener::from_std(listener)?;
	let (stream, _) = listener.accept().await?;
	let (reader, mut writer) = stream.into_split();
	let mut lines = BufReader::new(reader).lines();

	writer
		.write_all(b"220 localhost ESMTP\r\n")
		.await?;

	loop {
		let line = lines
			.next_line()
			.await?
			.ok_or_else(|| err!("SMTP client closed before sending a message"))?;

		match line.as_str() {
			| _ if line.starts_with("EHLO ") || line.starts_with("HELO ") =>
				writer.write_all(b"250 localhost\r\n").await?,
			| "DATA" => {
				writer
					.write_all(b"354 End data with <CRLF>.<CRLF>\r\n")
					.await?;

				let mut message = String::with_capacity(1024);

				loop {
					let line = lines
						.next_line()
						.await?
						.ok_or_else(|| err!("SMTP client closed during message data"))?;

					if line == "." {
						break;
					}

					message.push_str(line.strip_prefix("..").unwrap_or(&line));
					message.push_str("\r\n");
				}

				writer
					.write_all(b"250 Message accepted\r\n")
					.await?;

				return Ok(message);
			},
			| "QUIT" => {
				writer.write_all(b"221 Bye\r\n").await?;

				return Err!("SMTP client quit before sending a message");
			},
			| _ => writer.write_all(b"250 OK\r\n").await?,
		}
	}
}

fn validation_link(message: &str) -> Result<Url> {
	let (headers, body) = message
		.split_once("\r\n\r\n")
		.ok_or_else(|| err!("verification email omitted its body"))?;

	let body = match headers {
		| _ if headers.contains("Content-Transfer-Encoding: quoted-printable") => {
			let body = decode(body, ParseMode::Strict)
				.map_err(|error| err!("invalid quoted-printable email body: {error}"))?;

			String::from_utf8(body)
				.map(Cow::Owned)
				.map_err(|error| err!("invalid UTF-8 email body: {error}"))?
		},
		| _ if headers.contains("Content-Transfer-Encoding: 7bit") => Cow::Borrowed(body),
		| _ => return Err!("verification email used an unsupported transfer encoding"),
	};

	let link = body
		.split_once("href=\"")
		.and_then(|(_, tail)| tail.split_once('"'))
		.map(|(link, _)| link)
		.ok_or_else(|| err!("verification email omitted its validation link"))?;

	let link = link.replace("&amp;", "&");

	Url::parse(&link).map_err(Into::into)
}

pub(super) async fn view_confirmation(
	client: &Client,
	base: &str,
	sid: &str,
	client_secret: &str,
	token: &str,
) -> Result<String> {
	let url = format!("{base}/_tuwunel/3pid/email/validate");
	let query = [("sid", sid), ("client_secret", client_secret), ("token", token)];

	let response = client
		.get(url)
		.header("connection", "close")
		.query(&query)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	assert_eq!(status, 200, "email confirmation GET: {body}");

	Ok(body)
}

pub(super) async fn confirm_email(
	client: &Client,
	base: &str,
	sid: &str,
	client_secret: &str,
	token: &str,
) -> Result {
	let url = format!("{base}/_tuwunel/3pid/email/validate");
	let form = [("sid", sid), ("client_secret", client_secret), ("token", token)];

	let response = client
		.post(&url)
		.header("connection", "close")
		.form(&form)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	assert_eq!(status, 200, "email confirmation POST: {body}");
	assert!(body.contains("Email verified"), "email confirmation failed: {body}");

	let response = client
		.post(&url)
		.header("connection", "close")
		.form(&form)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	assert_eq!(status, 200, "email confirmation replay POST: {body}");
	assert!(!body.contains("Email verified"), "email confirmation replay succeeded");
	assert!(
		body.contains("This verification link is invalid or has expired."),
		"email confirmation replay did not use the generic failure page: {body}"
	);

	Ok(())
}

pub(super) async fn authenticated_password_change(
	AuthenticatedPasswordChange {
		client,
		base,
		user_id,
		access_token,
		old_password,
		new_password,
		session,
	}: AuthenticatedPasswordChange<'_>,
) -> Result<(u16, String)> {
	let body = json!({
		"new_password": new_password,
		"logout_devices": false,
		"auth": {
			"type": "m.login.password",
			"identifier": {
				"type": "m.id.user",
				"user": user_id.as_str(),
			},
			"password": old_password,
			"session": session,
		},
	});

	let response = client
		.post(format!("{base}/_matrix/client/v3/account/password"))
		.bearer_auth(access_token)
		.header("connection", "close")
		.json(&body)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
}

pub(super) async fn jwt_password_change(
	client: &Client,
	base: &str,
	access_token: Option<&str>,
	jwt: &str,
	new_password: &str,
) -> Result<(u16, String)> {
	let body = json!({
		"new_password": new_password,
		"logout_devices": false,
		"auth": {
			"type": "org.matrix.login.jwt",
			"token": jwt,
		},
	});

	let mut request = client
		.post(format!("{base}/_matrix/client/v3/account/password"))
		.header("connection", "close")
		.json(&body);

	if let Some(access_token) = access_token {
		request = request.bearer_auth(access_token);
	}

	let response = request.send().await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
}

pub(super) async fn password_login(
	client: &Client,
	base: &str,
	user_id: &UserId,
	password: &str,
) -> Result<(u16, String)> {
	let body = json!({
		"type": "m.login.password",
		"identifier": {
			"type": "m.id.user",
			"user": user_id.as_str(),
		},
		"password": password,
	});

	let response = client
		.post(format!("{base}/_matrix/client/v3/login"))
		.header("connection", "close")
		.json(&body)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
}

pub(super) async fn reset_password(
	client: &Client,
	base: &str,
	Reset {
		sid,
		client_secret,
		new_password,
		logout_devices,
		substitution,
	}: Reset<'_>,
) -> Result<(u16, String)> {
	let auth = EmailIdentityAuth {
		kind: "m.login.email.identity",
		threepid_creds: ThreepidCredentials { sid, client_secret },
		session: substitution.session,
	};

	let identifier = UserIdentifier {
		kind: "m.id.user",
		user: substitution.user,
	};

	let body = PasswordBody {
		auth,
		new_password,
		logout_devices,
		user_id: substitution.user,
		email: substitution.email,
		device_id: substitution.device,
		identifier,
	};

	let response = client
		.post(format!("{base}/_matrix/client/v3/account/password"))
		.header("connection", "close")
		.json(&body)
		.send()
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
}

pub(super) async fn wait_until_ready(client: &Client, base: &str) -> Result {
	let url = format!("{base}/_matrix/client/versions");

	timeout(Duration::from_secs(10), async {
		loop {
			if client
				.get(&url)
				.header("connection", "close")
				.send()
				.await
				.is_ok()
			{
				break;
			}

			sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.map_err(|_| err!("server listener did not become ready"))?;

	Ok(())
}
