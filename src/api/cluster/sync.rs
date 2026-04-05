use std::{convert::Infallible, time::Duration};

use axum::{
	body::Body,
	extract::{Query, State},
	http::{StatusCode, header},
	response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt, channel::mpsc};
use serde::Deserialize;
use tokio::time::sleep;
use tuwunel_core::{Err, Error, Result};
use tuwunel_database::{WalFrame, is_wal_gap_error};

/// Query parameters for `GET /_tuwunel/cluster/wal`.
#[derive(Debug, Deserialize)]
pub(crate) struct WalParams {
	/// Last sequence number the secondary successfully applied.
	/// Omit (or pass `0`) to start from the current WAL head.
	since: Option<u64>,
}

/// `GET /_tuwunel/cluster/sync?since=N`
///
/// Streams WAL frames to the secondary. The response body is a sequence of
/// length-prefixed [`WalFrame`] wire encodings (see `engine/replication.rs`).
///
/// Returns:
/// - `200 OK` with a streaming `application/octet-stream` body on success.
/// - `410 Gone` when the requested `since` sequence is older than the oldest
///   retained WAL segment. The secondary must full-resync from a checkpoint.
#[tracing::instrument(
	level = "debug",
	ret(level = "trace"),
	skip_all,
	fields(?params),
)]
pub(crate) async fn get_sync(
	State(services): State<crate::State>,
	Query(params): Query<WalParams>,
) -> impl IntoResponse {
	let since = params.since.unwrap_or(0);
	let db = services.db.clone();
	let interval_ms = services
		.server
		.config
		.rocksdb_replication_interval_ms;

	// Eagerly check for a WAL gap before opening the streaming response.
	let gap_check: Result = services
		.server
		.runtime()
		.spawn_blocking({
			let db = db.clone();
			move || db.engine.wal_frame_iter(since).map(drop)
		})
		.await
		.expect("spawn_blocking panicked in gap check");

	if let Err(ref e) = gap_check {
		if is_wal_gap_error(e) {
			return Err!(HttpJson(GONE, {
				"error": "WAL gap: secondary must re-sync from a fresh checkpoint"
			}));
		}

		return Err!(HttpJson(INTERNAL_SERVER_ERROR, {
			"error": format!("WAL iterator error: {e}")
		}));
	}

	// Channel that bridges the blocking WAL reader with the async HTTP body.
	let (mut tx, rx) = mpsc::channel::<Bytes>(256);
	services.server.runtime().spawn(async move {
		let mut seq = since;
		loop {
			// Drain all available WAL frames in a blocking thread.
			let db_ = db.clone();
			let result = services
				.server
				.runtime()
				.spawn_blocking(move || -> Result<(Vec<Bytes>, u64)> {
					let mut frames: Vec<Bytes> = Vec::new();
					let mut next_seq = seq;
					if let Ok(iter) = db_.engine.wal_frame_iter(seq) {
						for frame in iter.flatten() {
							next_seq = frame.next_resume_seq();
							frames.push(Bytes::from(frame.encode_to_vec()?));
						}
					}

					Ok((frames, next_seq))
				})
				.await;

			let Ok(Ok((frames, next_seq))) = result else {
				break; // spawn_blocking panicked
			};

			let advanced = next_seq != seq;
			seq = next_seq;

			for encoded in frames {
				if tx.send(encoded).await.is_err() {
					return Ok::<_, Error>(()); // client disconnected
				}
			}

			// Always emit a heartbeat so the secondary can tell the primary is alive.
			// When no data was produced, sleep first to avoid a busy-loop.
			if !advanced {
				sleep(Duration::from_millis(interval_ms)).await;
			}

			let db = db.clone();
			let hb_seq = services
				.server
				.runtime()
				.spawn_blocking(move || db.engine.current_sequence())
				.await
				.unwrap_or(seq);

			let hb = WalFrame::heartbeat(hb_seq);
			if tx
				.send(Bytes::from(hb.encode_to_vec()?))
				.await
				.is_err()
			{
				return Ok::<_, Error>(()); // client disconnected
			}
		}

		Ok::<_, Error>(())
	});

	Ok(Response::builder()
		.status(StatusCode::OK)
		.header(header::CONTENT_TYPE, "application/octet-stream")
		.header("x-tuwunel-role", "primary")
		.body(Body::from_stream(rx.map(Ok::<_, Infallible>)))
		.expect("Failed to build WAL streaming response"))
}
