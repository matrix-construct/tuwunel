#![cfg(test)]
#![cfg(unix)]

use std::{fs::create_dir_all, time::Duration};

use futures::future::join;
use serde_json::{Value, from_slice, json};
use tokio::{
	io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, copy},
	net::{UnixListener, UnixStream},
	time::timeout,
};
use tuwunel::Args;
use tuwunel_core::{
	Err, Error, Result,
	ruma::{
		api::{
			appservice::{
				Namespaces, Registration, RegistrationInit,
				event::push_events::v1::Request as PushEvents,
				ping::send_ping::v1::Request as Ping,
			},
			error::ErrorKind,
		},
		serde::Raw,
	},
};
use tuwunel_service::Services;

use self::fixture::boot;

#[expect(dead_code)] // Only the readiness probe is used from the client harness.
mod client;
mod fixture;

const HS_TOKEN: &str = "as1-secret-hs-token";

#[test]
fn appservice_unix_socket() -> Result {
	boot(
		"as1",
		[
			r#"proxy.by_domain=[{url="http://proxy-user:proxy-secret@proxy.example:8080",include=["localhost"]}]"#,
		],
		exercise,
	)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, _: &str) -> Result {
	let directory = Args::test_database_path("as1").join("s");

	create_dir_all(&directory)?;

	let path = directory.join("a.sock");
	let listener = UnixListener::bind(&path)?;

	for prefix in ["unix://", "unix:"] {
		let url = format!("{prefix}{}", path.display());
		let ping = services
			.appservice
			.ping(registration(&url), Ping::new());

		let (pinged, received) = timeout(Duration::from_secs(5), join(ping, receive(&listener)))
			.await
			.expect("ping must reach the Unix socket");

		let (received, _) = received?;

		assert!(
			pinged.is_ok() && matches_request(&received, "POST /_matrix/app/v1/ping"),
			"{prefix}"
		);
	}

	let url = format!("unix://{}", path.display());
	let event = json!({
		"type": "m.room.message",
		"event_id": "$as1:example.org",
		"room_id": "!as1:example.org",
		"sender": "@as1:example.org",
		"origin_server_ts": 1,
		"content": { "msgtype": "m.text", "body": "Unix transaction" },
	});

	let raw = Raw::new(&event)?;
	let request = PushEvents::new("as1".into(), vec![raw.cast_ref_unchecked().clone()]);
	let sent = services
		.appservice
		.send_request(registration(&url), request);

	let (sent, received) = timeout(Duration::from_secs(5), join(sent, receive(&listener)))
		.await
		.expect("transaction must reach the Unix socket");

	let (received, body) = received?;

	assert!(sent.is_ok() && matches_request(&received, "PUT /_matrix/app/v1/transactions/as1"));
	assert_eq!(body, json!({ "events": [event] }), "transaction payload");

	let missing = format!("unix://{}", directory.join("b.sock").display());
	let failed = services
		.appservice
		.ping(registration(&missing), Ping::new())
		.await;

	assert!(failed.is_err_and(|error| {
		matches!(&error, Error::Request(ErrorKind::ConnectionFailed, ..))
			&& !format!("{error:?}").contains(HS_TOKEN)
	}));

	for url in ["unix://run/a.sock", "unix:run/a.sock"] {
		assert!(
			services
				.appservice
				.register_appservice(registration(url))
				.await
				.is_err(),
			"{url}"
		);
	}

	Ok(())
}

fn registration(url: &str) -> Registration {
	RegistrationInit {
		id: "as1".to_owned(),
		url: Some(url.to_owned()),
		as_token: "as1-as-token".to_owned(),
		hs_token: HS_TOKEN.to_owned(),
		sender_localpart: "as1".to_owned(),
		namespaces: Namespaces::new(),
		rate_limited: None,
		protocols: None,
	}
	.into()
}

#[tracing::instrument(level = "debug", skip_all)]
async fn receive(listener: &UnixListener) -> Result<(String, Value)> {
	let (mut socket, _) = listener.accept().await?;
	let request = read_request(&mut socket).await?;

	socket
		.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
		.await?;

	Ok(request)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn read_request(socket: &mut UnixStream) -> Result<(String, Value)> {
	let mut reader = BufReader::new(socket); // Header reads may buffer part of the body.
	let head = read_head(&mut reader).await?;

	assert!(
		!head
			.lines()
			.filter_map(|line| line.split_once(':'))
			.any(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
			&& !head.contains("proxy-user")
			&& !head.contains("proxy-secret"),
		"proxy credentials must not reach the Unix socket"
	);

	let length = head
		.lines()
		.filter_map(|line| line.split_once(':'))
		.find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
		.map(|(_, length)| length.trim().parse())
		.transpose()?
		.unwrap_or(0);

	let body = read_body(&mut reader, length).await?;
	let body = from_slice(&body)?;

	Ok((head, body))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn read_head(reader: &mut BufReader<&mut UnixStream>) -> Result<String> {
	let mut head = String::new();

	while !head.ends_with("\r\n\r\n") {
		if reader.read_line(&mut head).await? == 0 {
			return Err!("Appservice request ended before its headers");
		}
	}

	Ok(head)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn read_body(reader: &mut BufReader<&mut UnixStream>, length: u64) -> Result<Vec<u8>> {
	let mut body = Vec::new();
	let copied = copy(&mut reader.take(length), &mut body).await?;

	if copied != length {
		return Err!("Appservice request ended before its body");
	}

	Ok(body)
}

fn matches_request(request: &str, endpoint: &str) -> bool {
	request.lines().next().is_some_and(|line| {
		line.strip_prefix(endpoint)
			.is_some_and(|rest| rest.starts_with(['?', ' ']))
	}) && request
		.lines()
		.filter_map(|line| line.split_once(':'))
		.any(|(name, value)| {
			name.eq_ignore_ascii_case("authorization")
				&& value
					.trim()
					.strip_prefix("Bearer ")
					.eq(&Some(HS_TOKEN))
		})
}
