#![cfg(test)]

use reqwest::StatusCode;
use serde_json::{Value, json};
use tuwunel_core::Result;

use self::client::{Client, register};

mod client;
mod fixture;

const ALICE_TOKEN: &str = "mutual-rooms-alice-token-0000000000";
const BOB_TOKEN: &str = "mutual-rooms-bob-token-000000000000";
const CAROL_TOKEN: &str = "mutual-rooms-carol-token-0000000000";

#[test]
fn stable_and_unstable_mutual_rooms() -> Result {
	fixture::boot("mutual-rooms", [], async |services, base| {
		register(services, "alice", ALICE_TOKEN).await?;
		let bob = register(services, "bob", BOB_TOKEN).await?;
		let carol = register(services, "carol", CAROL_TOKEN).await?;
		let alice = Client { services, base, token: ALICE_TOKEN };
		let other = Client { services, base, token: BOB_TOKEN };
		let room = alice
			.create_room(&json!({"preset": "public_chat"}))
			.await?;

		other
			.post(&format!("join/{room}"), &json!({}))
			.await?;

		for (prefix, counted) in [("v1/", true), ("unstable/uk.half-shot.msc2666/user/", false)] {
			let url = alice.client_url(prefix, &format!("mutual_rooms?user_id={bob}"));
			let http = &services.client.clients.default;

			assert_eq!(http.get(&url).send().await?.status(), StatusCode::UNAUTHORIZED);
			let body: Value = http
				.get(&url)
				.bearer_auth(alice.token)
				.send()
				.await?
				.error_for_status()?
				.json()
				.await?;

			assert_eq!(body["joined"], json!([room]));
			assert_eq!(body.get("count"), counted.then_some(&json!(1)));
			let empty_from: Value = http
				.get(format!("{url}&from="))
				.bearer_auth(alice.token)
				.send()
				.await?
				.error_for_status()?
				.json()
				.await?;

			assert_eq!(body, empty_from);
		}

		let empty: Value = services
			.client
			.clients
			.default
			.get(alice.client_url("v1/", &format!("mutual_rooms?user_id={carol}")))
			.bearer_auth(alice.token)
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		assert_eq!(empty["count"], 0);
		Ok(())
	})
}
