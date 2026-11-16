#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result, err, implement,
	ruma::{EventId, OwnedEventId, RoomId},
};
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, register},
	fixture::boot,
};

mod client;
mod fixture;

#[path = "client/event.rs"]
mod event;

#[expect(dead_code)] // Only the raw GET is used from the add-on.
#[path = "client/get.rs"]
mod get;

#[expect(dead_code)] // Only the self-membership action is used from the add-on.
#[path = "client/membership.rs"]
mod membership;

#[path = "client/put.rs"]
mod put;

#[path = "client/redact.rs"]
mod redact;

#[path = "client/thread.rs"]
mod thread;

const READER_TOKEN: &str = "bundle-requester-view-reader-token";
const IGNORED_TOKEN: &str = "bundle-requester-view-ignored-token";
const VISIBLE_TOKEN: &str = "bundle-requester-view-visible-token";

#[test]
fn requester_view_hides_ignored_and_redacted_relations() -> Result {
	boot("bundle-requester-view", [], exercise)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str) -> Result {
	let reader_id = register(services, "viewreader", READER_TOKEN).await?;
	let ignored_id = register(services, "viewignored", IGNORED_TOKEN).await?;

	register(services, "viewvisible", VISIBLE_TOKEN).await?;

	let reader = Client { services, base, token: READER_TOKEN };
	let ignored = Client { services, base, token: IGNORED_TOKEN };
	let visible = Client { services, base, token: VISIBLE_TOKEN };

	reader
		.set_ignored(&reader_id, &ignored_id)
		.await?;

	relations_view(&reader, &ignored, &visible).await
}

/// MSC2675: `/relations` hides children from an ignored sender, and redacted
/// children from every requester.
///
/// Recursion inherits both filters, a hidden child at `to` still ends the page,
/// and an ignored user's state event stays, as does every unredacted child for
/// a requester who ignores nobody.
#[tracing::instrument(level = "debug", skip_all)]
async fn relations_view(
	reader: &Client<'_>,
	ignored: &Client<'_>,
	visible: &Client<'_>,
) -> Result {
	// Members may send state, so the ignored user can relate a state event.
	let open_state = json!({
		"preset": "public_chat",
		"power_level_content_override": { "state_default": 0 },
	});

	let room = visible.create_room(&open_state).await?;

	reader.act(&room, "join").await?;
	ignored.act(&room, "join").await?;

	let root = visible
		.send_text(&room, "relations-root", "relations root")
		.await?;

	let shown = visible
		.react(&room, "relations-shown", &root, "a")
		.await?;

	let ignored_reaction = ignored
		.react(&room, "relations-ignored", &root, "a")
		.await?;

	let ignored_edit = ignored
		.replace(&room, "relations-ignored-edit", &root)
		.await?;

	let reply = visible
		.reply(&room, "relations-reply", &root)
		.await?;

	let redacted = visible
		.react(&room, "relations-redacted", &root, "b")
		.await?;

	// Positions are read before the redaction hides the reaction from every listing.
	let listed = visible.relations(&room, &root, "?dir=f").await?;
	let redacted_at = field(&listed, "next_batch")?.to_owned();
	let first_two = visible
		.relations(&room, &root, "?dir=f&limit=2")
		.await?;

	let ignored_at = field(&first_two, "next_batch")?.to_owned();

	visible
		.redact(&room, &redacted, "relations-redaction")
		.await?;

	let later = visible
		.react(&room, "relations-later", &root, "c")
		.await?;

	let relating = json!({ "m.relates_to": { "rel_type": "m.reference", "event_id": root } });
	let ignored_state = ignored
		.put_event(&format!("rooms/{room}/state/org.example.relating/relating"), &relating)
		.await?;

	let ignored_nested = ignored
		.react(&room, "relations-ignored-nested", &reply, "a")
		.await?;

	let shown_nested = visible
		.react(&room, "relations-shown-nested", &reply, "a")
		.await?;

	let all = reader.relations(&room, &root, "").await?;
	let all = chunk_ids(&all)?;

	assert!(all.contains(&shown.as_str()), "a visible reaction is missing");
	assert!(all.contains(&reply.as_str()), "a visible thread reply is missing");
	assert!(
		all.contains(&ignored_state.as_str()),
		"an ignored user's state event is missing"
	);
	assert!(!all.contains(&ignored_reaction.as_str()), "an ignored reaction was served");
	assert!(!all.contains(&ignored_edit.as_str()), "an ignored edit was served");
	assert!(!all.contains(&redacted.as_str()), "a redacted reaction was served");

	let annotations = reader
		.relations(&room, &root, "/m.annotation")
		.await?;

	let annotations = chunk_ids(&annotations)?;

	assert_eq!(annotations, [later.as_str(), shown.as_str()], "annotations kept a hidden one");

	let recursed = reader
		.relations(&room, &root, "?recurse=true")
		.await?;

	let recursed = chunk_ids(&recursed)?;

	assert!(recursed.contains(&shown_nested.as_str()), "recursion lost a visible reaction");
	assert!(
		!recursed.contains(&ignored_nested.as_str()),
		"recursion served an ignored reaction",
	);

	assert!(!recursed.contains(&ignored_edit.as_str()), "recursion served an ignored edit");
	assert!(!recursed.contains(&redacted.as_str()), "recursion served a redacted reaction");

	let to_ignored = reader
		.relations(&room, &root, &format!("?dir=f&to={ignored_at}"))
		.await?;

	assert_eq!(chunk_ids(&to_ignored)?, [shown.as_str()], "a hidden `to` did not end the page");

	let to_redacted = reader
		.relations(&room, &root, &format!("?dir=f&to={redacted_at}"))
		.await?;

	assert_eq!(
		chunk_ids(&to_redacted)?,
		[shown.as_str(), reply.as_str()],
		"a redacted `to` did not end the page",
	);

	let control = visible.relations(&room, &root, "").await?;
	let control = chunk_ids(&control)?;

	assert!(
		control.contains(&ignored_reaction.as_str()) && control.contains(&ignored_edit.as_str()),
		"a requester ignoring nobody lost another user's relations",
	);

	assert!(!control.contains(&redacted.as_str()), "a redacted reaction was served");

	Ok(())
}

fn chunk_ids(response: &Value) -> Result<Vec<&str>> {
	response["chunk"]
		.as_array()
		.ok_or_else(|| err!("relations response omitted its chunk"))?
		.iter()
		.map(|event| {
			event["event_id"]
				.as_str()
				.ok_or_else(|| err!("related event omitted its id"))
		})
		.collect()
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn relations(&self, room_id: &RoomId, event_id: &EventId, rest: &str) -> Result<Value> {
	let path = format!("rooms/{room_id}/relations/{event_id}{rest}");
	let response = self
		.get_url(&self.client_url("v1/", &path))
		.await?;

	accepted(response).await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn react(
	&self,
	room_id: &RoomId,
	txn_id: &str,
	target: &EventId,
	key: &str,
) -> Result<OwnedEventId> {
	let content = json!({
		"m.relates_to": { "rel_type": "m.annotation", "event_id": target, "key": key },
	});

	let path = format!("rooms/{room_id}/send/m.reaction/{txn_id}");

	self.put_event(&path, &content).await
}

#[implement(Client, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn replace(
	&self,
	room_id: &RoomId,
	txn_id: &str,
	target: &EventId,
) -> Result<OwnedEventId> {
	let content = json!({
		"msgtype": "m.text",
		"body": "* replaced",
		"m.new_content": { "msgtype": "m.text", "body": "replaced" },
		"m.relates_to": { "rel_type": "m.replace", "event_id": target },
	});

	let path = format!("rooms/{room_id}/send/m.room.message/{txn_id}");

	self.put_event(&path, &content).await
}
