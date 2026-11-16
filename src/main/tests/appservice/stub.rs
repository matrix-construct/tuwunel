//! A stub appservice that records the transactions it receives.
//!
//! It stands apart from the appservice registration add-on, so only the
//! binaries that read transactions compile it. The stub answers every
//! `PUT /transactions/{txnId}` with an empty `200` and hands on the
//! transaction ID and the body.

use std::{str::from_utf8, time::Duration};

use serde_json::Value;
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	sync::mpsc::{UnboundedReceiver, UnboundedSender},
	time::timeout,
};
use tuwunel_core::{
	Result, err,
	ruma::api::appservice::{Namespace, Namespaces, Registration, RegistrationInit},
};
use tuwunel_service::Services;

/// The registration of one stub appservice.
///
/// Its tokens are derived from its id, so a test supplies none.
pub(crate) struct Stub<'a> {
	/// Registration id; the tokens are derived from it.
	pub(crate) id: &'a str,

	/// Where the server sends the appservice's transactions.
	pub(crate) url: &'a str,

	/// Localpart of the appservice's own user.
	pub(crate) sender_localpart: &'a str,

	/// Pattern of the user ids it claims, exclusively.
	pub(crate) users: &'a str,

	/// Whether it opts into the MSC3202 transaction extensions.
	pub(crate) msc3202: bool,
}

/// Register a stub appservice directly with the running server.
///
/// Its transactions go to the stub's URL, and the token it names
/// authenticates from the next request without a config reload.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn register(
	services: &Services,
	&Stub {
		id,
		url,
		sender_localpart,
		users,
		msc3202,
	}: &Stub<'_>,
) -> Result {
	let namespaces = Namespaces {
		users: vec![Namespace::new(true, users.to_owned())],
		..Default::default()
	};

	let registration: Registration = RegistrationInit {
		id: id.to_owned(),
		url: Some(url.to_owned()),
		as_token: format!("{id}_as_token"),
		hs_token: format!("{id}_hs_token"),
		sender_localpart: sender_localpart.to_owned(),
		namespaces,
		rate_limited: None,
		protocols: None,
	}
	.into();

	let registration = Registration {
		msc3202_transaction_extensions: msc3202,
		..registration
	};

	services
		.appservice
		.register_appservice(registration)
		.await
}

/// The next transaction the stub received, as its ID and body.
///
/// None arriving within ten seconds fails the test.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn recv_txn(
	rx: &mut UnboundedReceiver<(String, Vec<u8>)>,
) -> Result<(String, Vec<u8>)> {
	timeout(Duration::from_secs(10), rx.recv())
		.await
		.map_err(|_| err!("timed out waiting for an appservice transaction"))?
		.ok_or_else(|| err!("stub appservice channel closed"))
}

/// A transaction body as JSON.
///
/// A body that is not JSON fails the test.
pub(crate) fn parse(body: &[u8]) -> Result<Value> {
	serde_json::from_slice(body)
		.map_err(|e| err!("appservice transaction body was not json: {e}"))
}

/// Serve the stub's transaction endpoint on `listener`.
///
/// Each `PUT /transactions/{txnId}` has its `{txnId}` path segment and body
/// handed to `tx`, then gets an empty `200`.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn serve(listener: TcpListener, tx: UnboundedSender<(String, Vec<u8>)>) {
	while let Ok((mut socket, _)) = listener.accept().await {
		if let Some((txn_id, body)) = read_request(&mut socket).await {
			tx.send((txn_id, body)).ok();
		}

		socket
			.write_all(
				b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
			)
			.await
			.ok();

		socket.flush().await.ok();
	}
}

#[tracing::instrument(level = "debug", skip_all)]
async fn read_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
	let mut buf = Vec::new();
	let mut chunk = [0_u8; 4096];

	loop {
		if let Some(head_end) = find(&buf, b"\r\n\r\n") {
			let content_length = content_length(&buf[..head_end])?;
			let body_start = head_end.checked_add(4)?;
			let body_end = body_start.checked_add(content_length)?;

			if buf.len() >= body_end {
				let txn_id = txn_id(&buf[..head_end])?;
				let body = buf[body_start..body_end].to_vec();

				return Some((txn_id, body));
			}
		}

		let read = socket.read(&mut chunk).await.ok()?;

		if read == 0 {
			return None;
		}

		buf.extend_from_slice(&chunk[..read]);
	}
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack
		.windows(needle.len())
		.position(|window| window == needle)
}

fn content_length(head: &[u8]) -> Option<usize> {
	from_utf8(head)
		.ok()?
		.lines()
		.find_map(|line| {
			line.split_once(':')
				.filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
		})
		.and_then(|(_, value)| value.trim().parse().ok())
}

fn txn_id(head: &[u8]) -> Option<String> {
	from_utf8(head)
		.ok()?
		.lines()
		.next()?
		.split(' ')
		.nth(1)?
		.rsplit('/')
		.next()?
		.split('?')
		.next()
		.map(ToOwned::to_owned)
}
