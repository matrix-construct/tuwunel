#![cfg(test)]

use std::fs::remove_dir_all;

use serde_json::{from_value, json};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_admin::{fini, init};
use tuwunel_core::{
	Err, Result,
	ruma::{CanonicalJsonObject, EventId, Mxc, room_id},
	utils::{
		BoolExt,
		time::{now, timepoint_from_epoch},
	},
};
use tuwunel_service::Services;

/// An empty eligible set is zero deletions, not an error: `delete_range` with
/// the purge_media_cache argument shape returns Ok(0), and the `media
/// delete-range` admin command reports zero deleted files.
#[test]
fn media_delete_range_empty_set() -> Result {
	let db_path = Args::test_database_path("media-delete-range-empty");

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_database_path(&db_path)
		.with_maintenance()
		.with_option("save_unredacted_events=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = async_start(&server).await?;

		init(&services.admin);

		let outcome = async {
			empty_delete_range_is_zero(&services).await?;
			delete_by_event_uses_retained_original(&services).await
		}
		.await;

		fini(&services.admin);
		server.server.shutdown()?;
		drop(services);

		async_run(&server).await?;
		async_stop(&server).await?;

		outcome
	});

	drop(runtime);

	remove_dir_all(&db_path).ok();

	result
}

async fn empty_delete_range_is_zero(services: &Services) -> Result {
	let cutoff = timepoint_from_epoch(now())?;

	let deleted = services
		.media
		.delete_range(cutoff, true, false, false)
		.await?;

	if deleted != 0 {
		return Err!("expected zero deletions over an empty media set: {deleted}");
	}

	match services
		.admin
		.command_in_place("media delete-range 7d --older-than".into())
		.await
	{
		| Ok(Some(output)) if output.as_str().contains("Deleted 0 total files.") => Ok(()),
		| Ok(None) => Err!("delete-range command produced no output"),
		| Ok(Some(output)) | Err(output) => {
			let output = output.as_str();

			Err!("unexpected delete-range output: {output}")
		},
	}
}

async fn delete_by_event_uses_retained_original(services: &Services) -> Result {
	let room = room_id!("!media-delete:localhost");
	let lock = services.state.mutex.lock(room).await;

	for (case, redacted, retained, media, expected) in [
		("live", false, false, true, "Deleted 1 total MXCs"),
		("retained", true, true, true, "Deleted 1 total MXCs"),
		("missing", true, false, true, "original unavailable"),
		("no-media", false, false, false, "found no MXC URLs"),
	] {
		let event_id = EventId::parse(format!("$media-delete-{case}:localhost"))?;
		let url = format!("mxc://{}/{case}", services.globals.server_name());
		let mxc: Mxc<'_> = url.as_str().try_into()?;
		let content = if media {
			json!({"msgtype": "m.file", "body": case, "url": url})
		} else {
			json!({"msgtype": "m.text", "body": case})
		};

		services
			.media
			.create(&mxc, None, None, Some("text/plain"), b"media-delete")
			.await?;

		let original: CanonicalJsonObject = from_value(json!({"content": content}))?;

		retained
			.then_async(async || {
				services
					.retention
					.save_original_pdu(&event_id, &original, &lock)
					.await;
			})
			.await;

		let live: CanonicalJsonObject = from_value(json!({
			"event_id": event_id,
			"room_id": room,
			"sender": services.globals.server_user,
			"type": "m.room.message",
			"content": if redacted { json!({}) } else { content },
			"unsigned": if redacted { json!({"redacted_because": {}}) } else { json!({}) },
			"prev_events": [], "auth_events": [], "depth": 1,
			"origin_server_ts": 1, "hashes": {"sha256": ""},
		}))?;

		services
			.timeline
			.add_pdu_outlier(&event_id, &live);

		let output = services
			.admin
			.command_in_place(format!("media delete-by-event --event-id {event_id}").into())
			.await;

		let (Ok(Some(output)) | Err(output)) = output else {
			return Err!("delete-by-event produced no output for {case}");
		};

		let output = output.as_str();

		if !output.contains(expected) {
			return Err!("unexpected delete-by-event output for {case}: {output}");
		}

		let stored = services.media.get_stored(&mxc).await;
		let deleted = media && (!redacted || retained);

		match stored {
			| Err(error) if deleted && error.is_not_found() => {},
			| Ok(_) if !deleted => {},
			| result => return Err!("unexpected media state for {case}: {result:?}"),
		}
	}

	Ok(())
}
