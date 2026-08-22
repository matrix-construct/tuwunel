#![cfg(test)]

use std::{
	env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
};

use serde_json::json;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Err, Result, utils::BoolExt};
use tuwunel_service::Services;

use self::{
	client::{Client, register, wait_until_ready},
	msc3664::{CONDITION_KIND, highlighted, notified, public_room},
};

mod client;
mod msc3664;

const AUTHOR_TOKEN: &str = "msc3664-disabled-author-access-token";
const READER_TOKEN: &str = "msc3664-disabled-reader-access-token";

/// Holds the default off state of the MSC3664 condition.
///
/// The server resolves no relations unless `msc3664_related_event_match` is
/// set, so a rule using the condition must match nothing at all, including the
/// relation-type-only shape that needs no related event to decide. Reactions
/// then stay suppressed by the default rule, as on any server without MSC3664.
///
/// The paired `msc3664_reaction_notify` binary is what makes this a gate test
/// rather than a restatement of that default rule: it drives the same rule
/// shape with the option set and requires it to notify.
#[test]
fn related_event_match_is_inert_by_default() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let port = listener.local_addr()?.port();

	let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
	let db_path = PathBuf::from(root).join(format!("tuwunel-msc3664-disabled-{}", process_id()));

	let mut args = Args::default_test(&["fresh", "cleanup"]);

	args.option.extend([
		format!("database_path={db_path:?}"),
		"address=[\"127.0.0.1\"]".to_owned(),
		format!("port={port}"),
		"listening=true".to_owned(),
	]);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let exercise = async {
			let outcome = exercise(&services, &base).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = tokio::join!(async_run(&server), exercise);

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(runtime);
	remove_dir_all(&db_path).ok();

	result
}

async fn exercise(services: &Services, base: &str) -> Result {
	wait_until_ready(services, base).await?;

	if services.config.msc3664_related_event_match {
		return Err!("msc3664_related_event_match must default to off");
	}

	let author = register(services, "msc3664offauthor", AUTHOR_TOKEN).await?;

	register(services, "msc3664offreader", READER_TOKEN).await?;

	let writer = Client { services, base, token: AUTHOR_TOKEN };
	let reader = Client { services, base, token: READER_TOKEN };
	let room = writer.create_room(&public_room()).await?;

	reader.join(&room).await?;

	if writer.condition_capability().await?.is_some() {
		return Err!("the condition is not evaluated but its capability is advertised");
	}

	// Both shapes the condition accepts: one needing the related event, and one
	// deciding on the relation type alone.
	let by_sender = json!({
		"conditions": [{
			"kind": CONDITION_KIND,
			"rel_type": "m.annotation",
			"key": "sender",
			"pattern": author.as_str(),
		}],
		"actions": ["notify", { "set_tweak": "highlight" }],
	});

	let by_relation = json!({
		"conditions": [{
			"kind": CONDITION_KIND,
			"rel_type": "m.annotation",
		}],
		"actions": ["notify"],
	});

	writer
		.set_push_rule("msc3664_reactions_to_me", &by_sender)
		.await?;

	writer
		.set_push_rule("msc3664_any_annotation", &by_relation)
		.await?;

	let mine = writer
		.send(
			&room,
			"m.room.message",
			"t1",
			&json!({
				"msgtype": "m.text",
				"body": "Dinner at 7?",
			}),
		)
		.await?;

	let before = services
		.pusher
		.notification_count(&author, &room)
		.await;

	// Positive control. Every other assertion here is a negative, which a
	// wedged or merely slow evaluator would satisfy for the wrong reason, so
	// prove evaluation is live in this process before trusting one.
	reader
		.send(
			&room,
			"m.room.message",
			"t2",
			&json!({
				"msgtype": "m.text",
				"body": "Works for me",
			}),
		)
		.await?;

	if notified(services, &author, &room, before.saturating_add(1))
		.await
		.is_false()
	{
		return Err!("push evaluation never ran, so the negative case below proves nothing");
	}

	let baseline = services
		.pusher
		.notification_count(&author, &room)
		.await;

	reader
		.send(
			&room,
			"m.reaction",
			"t3",
			&json!({
				"m.relates_to": {
					"rel_type": "m.annotation",
					"event_id": mine,
					"key": "👍",
				},
			}),
		)
		.await?;

	if notified(services, &author, &room, baseline.saturating_add(1)).await {
		return Err!("a reaction notified while msc3664_related_event_match was off");
	}

	if highlighted(services, &author, &room, 1).await {
		return Err!("a reaction highlighted while msc3664_related_event_match was off");
	}

	Ok(())
}
