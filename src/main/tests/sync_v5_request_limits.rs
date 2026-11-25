#![cfg(test)]

use reqwest::StatusCode;
use serde_json::{Map, Value, json, to_value};
use tuwunel_core::Result;
use tuwunel_service::{Services, sync::into_connection_key};

use self::client::{Client, field, register};

#[expect(dead_code)] // Only registration and sync are needed from the client harness.
mod client;
mod fixture;
mod sliding_sync;

const TOKEN: &str = "sliding-sync-request-limits-test-access-token";
const CONN: &str = "limits";

#[test]
fn oversized_requests_leave_live_and_stored_connection_unchanged() -> Result {
	fixture::boot(
		"sync-request-limits",
		["logging.enable=false", "presence.allow_local=true"],
		exercise,
	)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "limits", TOKEN).await?;
	let client = Client { services, base, token: TOKEN };
	let list = json!({"ranges": [], "timeline_limit": 0});
	let mut lists: Map<_, _> = (1..64)
		.map(|i| (i.to_string(), list.clone()))
		.collect();
	lists.insert("keep".into(), list.clone());
	let opened = client
		.sliding_sync(None, 0, &json!({"conn_id": CONN, "lists": lists}))
		.await?;
	let pos = field(&opened, "pos")?;
	let (user, device, _) = services.users.find_from_token(TOKEN).await?;
	let key = into_connection_key(user.clone(), Some(device.clone()), Some(CONN));
	let conn = services.sync.get_loaded_connection(&key).await?;
	let stored = services.db.get("userdeviceconnid_conn")?;
	let before = to_value(&*conn.lock().await)?;
	let bytes = stored.qry(&key).await?.to_vec();

	let tags: Vec<_> = (0..17).map(|i| format!("u.tag{i}")).collect();
	let invalid = json!({
		"conn_id": CONN,
		"lists": {"extra": {"ranges": [], "timeline_limit": 0, "filters": {"not_tags": tags}}},
		"extensions": {"typing": {"enabled": true}},
	});
	let extra = json!({"conn_id": CONN, "lists": {"extra": list}});
	for (from, body) in [(Some(pos), &invalid), (None, &invalid), (Some(pos), &extra)] {
		let reply = client
			.post_url(&client.sliding_sync_url(from, 0), body)
			.await?;
		assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
		assert_eq!(reply.json::<Value>().await?["errcode"], "M_INVALID_PARAM");
		assert_eq!(to_value(&*conn.lock().await)?, before);
		assert_eq!(stored.qry(&key).await?.as_ref(), bytes.as_slice());
	}
	// Copy the persisted state to a connection that has never been loaded.
	let cold = "cold";
	let cold_key = into_connection_key(user.clone(), Some(device), Some(cold));
	stored.put_raw(&cold_key, &bytes);
	let presence = services.db.get("userid_presenceid")?;
	let presence_before = presence.get(user.as_bytes()).await?.to_vec();
	let mut rejected = extra.clone();
	rejected["conn_id"] = json!(cold);
	let url = format!("{}&set_presence=offline", client.sliding_sync_url(Some(pos), 0));
	let reply = client.post_url(&url, &rejected).await?;
	assert_eq!(reply.status(), StatusCode::BAD_REQUEST);
	assert_eq!(reply.json::<Value>().await?["errcode"], "M_INVALID_PARAM");
	assert_eq!(
		(
			services
				.sync
				.is_connection_loaded(&cold_key)
				.await,
			presence.get(user.as_bytes()).await?.to_vec()
		),
		(false, presence_before)
	);
	let resumed = client
		.sliding_sync(Some(pos), 0, &json!({"conn_id": CONN}))
		.await?;
	assert!(resumed["lists"].get("keep").is_some());
	assert!(resumed["lists"].get("extra").is_none());
	let fresh = client.sliding_sync(None, 0, &extra).await?;
	assert!(fresh["lists"].get("extra").is_some());
	assert!(fresh["lists"].get("keep").is_none());
	Ok(())
}
