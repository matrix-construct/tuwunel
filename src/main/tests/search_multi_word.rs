#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{Result, err};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

mod client;
mod fixture;

#[path = "client/put.rs"]
mod put;

#[path = "client/message.rs"]
mod message;

const TOKEN: &str = "search-multi-word-user-access-token";

/// The room's messages, oldest first.
///
/// The last holds only `alpha`, so its list is longer than the later words'
/// in the searches for all three.
const MESSAGES: [&str; 10] = [
	"alpha beta gamma",
	"alpha only",
	"beta gamma",
	"Gamma, BETA and alpha!",
	"alpha beta",
	"go far to go fast",
	"alpha gamma beta delta",
	"go home",
	"gamma",
	"alpha",
];

/// Each search term with the indices of the messages it finds, newest first.
const SEARCHES: [(&str, &[usize]); 8] = [
	("alpha beta gamma", &[6, 3, 0]),
	("gamma ALPHA", &[6, 3, 0]),
	("go fast to go", &[5]),
	("alpha alpha", &[9, 6, 4, 3, 1, 0]),
	("gamma", &[8, 6, 3, 2, 0]),
	("Beta, BETA beta", &[6, 4, 3, 2, 0]),
	("alpha zeta", &[]),
	("?! ...", &[]),
];

/// A multi-word search finds the messages holding every word, newest first.
///
/// Each search is paged at several limits. A repeated word counts once, a
/// single word finds every message holding it, and a word no message holds,
/// or a term without words, leaves no hits.
#[test]
fn multi_word_search_finds_messages_with_every_word() -> Result {
	boot("search-multi-word", [], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "multiwordsearcher", TOKEN).await?;

	let client = Client { services, base, token: TOKEN };
	let room = client
		.create_room(&json!({"preset": "private_chat"}))
		.await?;

	let mut sent = Vec::new();
	for (txn, body) in MESSAGES.iter().enumerate() {
		let event_id = client
			.send_text(&room, &txn.to_string(), body)
			.await?;

		sent.push(event_id.to_string());
	}

	for (term, newest_first) in SEARCHES {
		let expected: Vec<_> = newest_first
			.iter()
			.map(|&i| sent[i].as_str())
			.collect();

		for limit in [1, 2, 10] {
			let found = search(&client, term, limit, expected.len()).await?;
			assert_eq!(found, expected, "search for {term:?} at limit {limit}");
		}
	}

	Ok(())
}

/// Reads every hit of one search, `limit` per page, checking each page.
async fn search(
	client: &Client<'_>,
	term: &str,
	limit: usize,
	total: usize,
) -> Result<Vec<String>> {
	let body = json!({"search_categories": {"room_events": {
		"search_term": term, "filter": {"limit": limit}
	}}});

	let mut hits = Vec::new();
	let mut cursor: Option<String> = None;
	for _ in 0..=total {
		let response: Value = client
			.services
			.client
			.clients
			.default
			.post(client.url("search"))
			.bearer_auth(client.token)
			.query(&[("next_batch", cursor.as_deref())])
			.json(&body)
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		let events = &response["search_categories"]["room_events"];
		assert_eq!(events["count"], json!(total), "count of {term:?}");

		let page = events["results"]
			.as_array()
			.ok_or_else(|| err!("search omitted results"))?;

		assert!(page.len() <= limit, "search for {term:?} exceeded its limit");
		for hit in page {
			hits.push(field(&hit["result"], "event_id")?.to_owned());
		}

		let Some(next) = events["next_batch"].as_str() else {
			return Ok(hits);
		};

		cursor = Some(next.to_owned());
	}

	Err(err!("search for {term:?} did not end"))
}
