use futures::StreamExt;
use ruma::room_id;
use tuwunel_core::{Result, config::Figment};
use tuwunel_database::SEP;

use super::{WORD_MAX_LEN, make_tokenid, tokenize};
use crate::test_utils::{fixture, pdu_id};

#[test]
fn lowercase_expansion_preserves_the_full_search_key() {
	let body = "İ".repeat(25);
	let word = tokenize(&body).next().expect("one accepted word");
	let pdu_id = pdu_id(42);
	let key = make_tokenid(1, &word, &pdu_id);
	let expected: Vec<_> = 1_u64
		.to_be_bytes()
		.into_iter()
		.chain("i\u{307}".repeat(25).bytes())
		.chain([SEP])
		.chain(pdu_id.as_bytes().iter().copied())
		.collect();

	assert_eq!(body.len(), WORD_MAX_LEN);
	assert!(word.len() > WORD_MAX_LEN);
	assert_eq!(key.as_slice(), expected);
	assert!(key.spilled());
}

#[tokio::test]
async fn lowercase_expansion_remains_searchable() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let services = &fixture.services;
	let room = room_id!("!search:localhost");
	let shortroomid = services
		.short
		.get_or_create_shortroomid(room)
		.await;

	let pdu = pdu_id(42);
	let body = "İ".repeat(25);
	let word = tokenize(&body).next().expect("one accepted word");

	services
		.search
		.index_pdu(shortroomid, &pdu, &body);

	let found: Vec<_> = services
		.search
		.search_pdu_ids_query_words(shortroomid, &word)
		.collect()
		.await;

	assert_eq!(found, [pdu]);
	services
		.search
		.deindex_pdu(shortroomid, &pdu, &body);

	let remaining = services
		.search
		.search_pdu_ids_query_words(shortroomid, &word)
		.count()
		.await;

	assert_eq!(remaining, 0);

	Ok(())
}
