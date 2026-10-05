#![cfg(test)]

use futures::{StreamExt, TryStreamExt, stream::try_unfold};
use serde_json::{Value, json};
use tuwunel_core::{Result, err, utils::IterStream};
use tuwunel_service::Services;

use self::{
	client::{Client, field, register},
	fixture::boot,
};

mod client;
mod fixture;

type Cursor = Option<(Option<String>, usize)>;
type Page = (Vec<String>, Cursor);

const TOKEN: &str = "search-pagination-user-access-token";
const HITS: usize = 15;

/// Pins global page limits, count totals, and lossless pagination across rooms.
///
/// Interleaved hits are paged at several limits and compared with send order.
#[test]
fn search_pagination_visits_each_hit_once() -> Result {
	let options: [&str; 0] = [];

	boot("search-pagination", options, exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	register(services, "searcher", TOKEN).await?;

	let client = Client { services, base, token: TOKEN };
	let rooms: Vec<_> = (0..3)
		.stream()
		.then(async |_| {
			client
				.create_room(&json!({"preset": "private_chat"}))
				.await
		})
		.try_collect()
		.await?;

	// Serial sends establish an independent global ordering across rooms.
	let expected: Vec<_> = rooms
		.iter()
		.cycle()
		.take(HITS)
		.enumerate()
		.stream()
		.then(async |(i, room)| {
			let path = format!("rooms/{room}/send/m.room.message/{i}");
			let body = json!({"msgtype": "m.text", "body": "paginationneedle"});
			let response: Value = services
				.client
				.clients
				.default
				.put(client.url(&path))
				.bearer_auth(TOKEN)
				.json(&body)
				.send()
				.await?
				.error_for_status()?
				.json()
				.await?;

			field(&response, "event_id").map(str::to_owned)
		})
		.try_collect()
		.await?;

	for limit in [1, 4, 7] {
		let body = json!({"search_categories": {"room_events": {
			"search_term": "paginationneedle", "filter": {"limit": limit}
		}}});

		let pages: Vec<Vec<String>> =
			try_unfold(Some((None, 0)), async |state| page(&client, &body, limit, state).await)
				.try_collect()
				.await?;

		let actual: Vec<_> = pages.into_iter().flatten().collect();

		assert_eq!(actual.len(), HITS);
		assert!(actual.iter().eq(expected.iter().rev()), "search lost or repeated hits");
	}

	Ok(())
}

async fn page(
	client: &Client<'_>,
	body: &Value,
	limit: usize,
	state: Cursor,
) -> Result<Option<Page>> {
	let Some((cursor, page)) = state else { return Ok(None) };

	assert!(page <= HITS, "search cursor failed to terminate");

	let url = client.url("search");
	let response: Value = client
		.services
		.client
		.clients
		.default
		.post(url)
		.bearer_auth(client.token)
		.query(&[("next_batch", cursor.as_deref())])
		.json(body)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let events = &response["search_categories"]["room_events"];

	assert_eq!(events["count"], json!(HITS));

	let hits = events["results"]
		.as_array()
		.ok_or_else(|| err!("search omitted results"))?;

	assert!(hits.len() <= limit, "search exceeded its global limit");

	let ids: Result<Vec<_>> = hits
		.iter()
		.map(|hit| field(&hit["result"], "event_id").map(str::to_owned))
		.collect();

	let next = events["next_batch"]
		.as_str()
		.map(|cursor| (Some(cursor.to_owned()), page.saturating_add(1)));

	Ok(Some((ids?, next)))
}
