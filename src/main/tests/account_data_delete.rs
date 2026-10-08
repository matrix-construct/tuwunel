#![cfg(test)]

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tuwunel_core::{
	Result,
	ruma::{RoomId, UserId},
};
use tuwunel_service::Services;

use self::{
	appservice::{Bridge, register_appservice},
	client::{Client, register},
	fixture::boot,
};

mod appservice;
mod client;
mod fixture;

const OWNER_TOKEN: &str = "account-data-delete-owner-token-00";
const BRIDGE_TOKEN: &str = "account-data-delete-bridge-token-0";
const KIND: &str = "org.example.deletable";

#[test]
fn deletion_respects_protected_types_and_ownership() -> Result {
	boot("account-data-delete", ["log_enable=false"], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let owner = register(services, "owner", OWNER_TOKEN).await?;
	let bridged =
		register(services, "bridged_alice", "account-data-bridged-alice-token-0").await?;

	let bridge = Bridge {
		id: "account-data",
		token: BRIDGE_TOKEN,
		sender_localpart: "bridge_bot",
		users: "^@bridged_.*$",
		aliases: None,
	};

	register(services, "bridge_bot", "account-data-bridge-bot-user-token").await?;
	register_appservice(services, &bridge).await?;

	let ordinary = Client { services, base, token: OWNER_TOKEN };
	let appservice = Client { services, base, token: BRIDGE_TOKEN };

	let room = ordinary.create_room(&json!({})).await?;

	for room in [None, Some(room.as_ref())] {
		owned(&ordinary, &owner, room).await?;
		owned(&appservice, &bridged, room).await?;

		refused(&ordinary, &bridged, room, KIND, StatusCode::FORBIDDEN, "M_FORBIDDEN").await?;
		refused(&appservice, &owner, room, KIND, StatusCode::FORBIDDEN, "M_FORBIDDEN").await?;
	}

	Ok(())
}

async fn owned(client: &Client<'_>, user: &UserId, room: Option<&RoomId>) -> Result {
	for kind in ["m.fully_read", "m.push_rules"] {
		refused(client, user, room, kind, StatusCode::BAD_REQUEST, "M_BAD_JSON").await?;
	}

	allowed(client, user, room).await
}

async fn refused(
	client: &Client<'_>,
	user: &UserId,
	room: Option<&RoomId>,
	kind: &str,
	status: StatusCode,
	code: &str,
) -> Result {
	seed(client.services, user, room, kind).await?;

	let data = &client.services.account_data;
	let before = data.get_raw(room, user, kind).await?.to_vec();
	let count = data.last_count(room, user, None).await?;
	let path = path(user, room, kind);
	let (actual, body) = request(client, Method::DELETE, &path).await?;

	assert_eq!(actual, status, "{path}: {body}");
	assert_eq!(body["errcode"], code, "{path}");
	assert_eq!(data.get_raw(room, user, kind).await?.as_ref(), before, "{path}");
	assert_eq!(data.last_count(room, user, None).await?, count, "{path}");

	Ok(())
}

async fn allowed(client: &Client<'_>, user: &UserId, room: Option<&RoomId>) -> Result {
	seed(client.services, user, room, KIND).await?;

	let path = path(user, room, KIND);

	for _ in 0..2 {
		let (status, body) = request(client, Method::DELETE, &path).await?;

		assert_eq!(status, StatusCode::OK, "{path}: {body}");
		assert_eq!(body, json!({}), "{path}");

		let (status, body) = request(client, Method::GET, &path).await?;

		assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
		assert_eq!(body["errcode"], "M_NOT_FOUND", "{path}");
	}

	let stored = client
		.services
		.account_data
		.get_raw(room, user, KIND)
		.await?;

	let stored: Value = serde_json::from_slice(&stored)?;

	assert_eq!(stored["content"], json!({}));

	Ok(())
}

async fn seed(services: &Services, user: &UserId, room: Option<&RoomId>, kind: &str) -> Result {
	let event = json!({ "type": kind, "content": { "preserved": true } });

	services
		.account_data
		.update(room, user, kind.into(), &event)
		.await
}

fn path(user: &UserId, room: Option<&RoomId>, kind: &str) -> String {
	match room {
		| None => format!("user/{user}/account_data/{kind}"),
		| Some(room) => format!("user/{user}/rooms/{room}/account_data/{kind}"),
	}
}

async fn request(client: &Client<'_>, method: Method, path: &str) -> Result<(StatusCode, Value)> {
	let version = if method == Method::DELETE {
		"unstable/org.matrix.msc3391"
	} else {
		"v3"
	};
	let url = format!("{}/_matrix/client/{version}/{path}", client.base);
	let response = client
		.services
		.client
		.clients
		.default
		.request(method, url)
		.bearer_auth(client.token)
		.send()
		.await?;

	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}
