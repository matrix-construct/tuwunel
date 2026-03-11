//! Primary-side HTTP handlers for WAL-based RocksDB replication.
//!
//! Endpoints (all protected by `check_replication_token` middleware):
//! - `GET  /_tuwunel/replication/status`        — current sequence number + role
//! - `GET  /_tuwunel/replication/wal?since=N`   — streaming WAL frame feed
//! - `GET  /_tuwunel/replication/checkpoint`    — full database checkpoint as tar
//! - `POST /_tuwunel/replication/promote`       — promote secondary to primary
//! - `POST /_tuwunel/replication/demote`        — demote primary back to secondary

use std::time::Duration;

use axum::{
	Json,
	body::Body,
	extract::{Query, State},
	http::{StatusCode, header},
	response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tuwunel_core::Result;
use tuwunel_database::{WalFrame, is_wal_gap_error};

/// Query parameters for `GET /_tuwunel/replication/wal`.
#[derive(Debug, Deserialize)]
pub(crate) struct WalParams {
	/// Last sequence number the secondary successfully applied.
	/// Omit (or pass `0`) to start from the current WAL head.
	pub since: Option<u64>,
}

/// `GET /_tuwunel/replication/status`
///
/// Returns the primary's current WAL sequence number and role.
pub(crate) async fn replication_status(
	State(services): State<crate::State>,
) -> impl IntoResponse {
	let db = services.db.clone();
	let seq = tokio::task::spawn_blocking(move || db.latest_wal_sequence())
		.await
		.unwrap_or(0);

	let role = if services.server.config.rocksdb_primary_url.is_some()
		&& !services.replication.is_promoted()
	{
		"secondary"
	} else {
		"primary"
	};

	axum::Json(serde_json::json!({
		"role": role,
		"latest_sequence": seq,
	}))
}

/// `GET /_tuwunel/replication/wal?since=N`
///
/// Streams WAL frames to the secondary. The response body is a sequence of
/// length-prefixed [`WalFrame`] wire encodings (see `engine/replication.rs`).
///
/// Returns:
/// - `200 OK` with a streaming `application/octet-stream` body on success.
/// - `410 Gone` when the requested `since` sequence is older than the oldest
///   retained WAL segment. The secondary must full-resync from a checkpoint.
pub(crate) async fn replication_wal(
	State(services): State<crate::State>,
	Query(params): Query<WalParams>,
) -> Response {
	let since = params.since.unwrap_or(0);
	let db = services.db.clone();
	let interval_ms = services.server.config.rocksdb_replication_interval_ms;

	// Eagerly check for a WAL gap before opening the streaming response.
	let gap_check: Result<()> = tokio::task::spawn_blocking({
		let db = db.clone();
		move || db.wal_frame_iter(since).map(drop)
	})
	.await
	.expect("spawn_blocking panicked in gap check");

	if let Err(ref e) = gap_check {
		if is_wal_gap_error(e) {
			return (
				StatusCode::GONE,
				"WAL gap: secondary must re-sync from a fresh checkpoint",
			)
				.into_response();
		}
		return (
			StatusCode::INTERNAL_SERVER_ERROR,
			format!("WAL iterator error: {e}"),
		)
			.into_response();
	}

	// Channel that bridges the blocking WAL reader with the async HTTP body.
	let (mut tx, rx) = futures::channel::mpsc::channel::<Bytes>(256);

	tokio::spawn(async move {
		let mut seq = since;

		loop {
			// Drain all available WAL frames in a blocking thread.
			let result = tokio::task::spawn_blocking({
				let db = db.clone();
				move || -> (Vec<Bytes>, u64) {
					let mut frames: Vec<Bytes> = Vec::new();
					let mut next_seq = seq;
					match db.wal_frame_iter(seq) {
						| Ok(iter) => {
							for item in iter {
								if let Ok(frame) = item {
									next_seq = frame.next_resume_seq();
									frames.push(Bytes::from(frame.encode()));
								}
							}
						},
						| Err(_) => {},
					}
					(frames, next_seq)
				}
			})
			.await;

			let (frames, next_seq) = match result {
				| Ok(pair) => pair,
				| Err(_) => break, // spawn_blocking panicked
			};

			let advanced = next_seq != seq;
			seq = next_seq;

			for encoded in frames {
				if tx.send(encoded).await.is_err() {
					return; // client disconnected
				}
			}

			// Always emit a heartbeat so the secondary can tell the primary is alive.
			// When no data was produced, sleep first to avoid a busy-loop.
			if !advanced {
				tokio::time::sleep(Duration::from_millis(interval_ms)).await;
			}

			let hb_seq = {
				let db = db.clone();
				tokio::task::spawn_blocking(move || db.latest_wal_sequence())
					.await
					.unwrap_or(seq)
			};
			let hb = WalFrame::heartbeat(hb_seq);
			if tx.send(Bytes::from(hb.encode())).await.is_err() {
				return; // client disconnected
			}
		}
	});

	let stream = rx.map(Ok::<_, std::convert::Infallible>);
	Response::builder()
		.status(StatusCode::OK)
		.header(header::CONTENT_TYPE, "application/octet-stream")
		.header("x-tuwunel-role", "primary")
		.body(Body::from_stream(stream))
		.expect("Failed to build WAL streaming response")
}

/// `GET /_tuwunel/replication/checkpoint`
///
/// Creates a RocksDB checkpoint of the primary's database and streams it as a
/// tar archive. The `X-Tuwunel-Checkpoint-Sequence` response header carries
/// the WAL sequence number at checkpoint creation time; the secondary uses
/// this as its initial `?since=` value when it begins WAL streaming.
///
/// The caller is responsible for pausing WAL consumption while restoring the
/// checkpoint and then resuming from `X-Tuwunel-Checkpoint-Sequence`.
pub(crate) async fn replication_checkpoint(
	State(services): State<crate::State>,
) -> Response {
	let db = services.db.clone();

	// Build the checkpoint and tar it in a blocking thread.
	let result = tokio::task::spawn_blocking(move || -> Result<(Bytes, u64)> {
		let tmp = tempfile_checkpoint_dir()?;
		let checkpoint_path = tmp.path().join("checkpoint");

		let seq = db.create_checkpoint(&checkpoint_path)?;

		// Build tar archive in memory.
		let mut archive_bytes: Vec<u8> = Vec::new();
		{
			let mut builder = tar::Builder::new(&mut archive_bytes);
			builder
				.append_dir_all("checkpoint", &checkpoint_path)
				.map_err(|e| tuwunel_core::err!(Database("{e}")))?;
			builder
				.finish()
				.map_err(|e| tuwunel_core::err!(Database("{e}")))?;
		}

		Ok((Bytes::from(archive_bytes), seq))
	})
	.await;

	match result {
		| Ok(Ok((bytes, seq))) => Response::builder()
			.status(StatusCode::OK)
			.header(header::CONTENT_TYPE, "application/x-tar")
			.header("x-tuwunel-checkpoint-sequence", seq.to_string())
			.body(Body::from(bytes))
			.expect("Failed to build checkpoint response"),

		| Ok(Err(e)) => (
			StatusCode::INTERNAL_SERVER_ERROR,
			format!("Checkpoint creation failed: {e}"),
		)
			.into_response(),

		| Err(e) => (
			StatusCode::INTERNAL_SERVER_ERROR,
			format!("Spawn_blocking panicked: {e}"),
		)
			.into_response(),
	}
}

/// `POST /_tuwunel/replication/promote`
///
/// Promotes this secondary to a standalone primary by stopping the replication
/// worker. After this call returns the instance accepts writes and no longer
/// tails the primary's WAL. The caller is responsible for updating the VIP or
/// load balancer to route client traffic to this node.
///
/// Returns:
/// - `200 OK` with `{"status":"promoted"}` on success.
/// - `409 Conflict` if this instance is already a primary (no `rocksdb_primary_url`
///   was configured, or it was already promoted).
pub(crate) async fn replication_promote(
	State(services): State<crate::State>,
) -> impl IntoResponse {
	if services.replication.is_promoted() {
		return (
			StatusCode::CONFLICT,
			axum::Json(serde_json::json!({"error": "already promoted"})),
		)
			.into_response();
	}

	if services.server.config.rocksdb_primary_url.is_none() {
		return (
			StatusCode::CONFLICT,
			axum::Json(serde_json::json!({"error": "not a secondary; no rocksdb_primary_url configured"})),
		)
			.into_response();
	}

	services.replication.promote();

	axum::Json(serde_json::json!({"status": "promoted"})).into_response()
}

/// Request body for `POST /_tuwunel/replication/demote`.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DemoteBody {
	/// URL of the new primary to replicate from (e.g. `http://host:8008`).
	pub primary_url: String,
}

/// `POST /_tuwunel/replication/demote`
///
/// Demotes this promoted primary back to a secondary that replicates from
/// `primary_url`. Resets the resume cursor and triggers a fresh checkpoint
/// bootstrap from the new primary — the worker restarts replication without
/// requiring a process restart.
///
/// Typical use case: the original primary comes back online after a failover
/// and needs to re-join the cluster as a secondary under the newly promoted
/// node.
///
/// Returns:
/// - `200 OK` with `{"status":"demoted","primary_url":"..."}` on success.
/// - `400 Bad Request` if `primary_url` is missing or empty.
/// - `409 Conflict` if this instance is not currently promoted (i.e. it is
///   already actively replicating or was never a secondary).
pub(crate) async fn replication_demote(
	State(services): State<crate::State>,
	Json(body): Json<DemoteBody>,
) -> impl IntoResponse {
	if body.primary_url.is_empty() {
		return (
			StatusCode::BAD_REQUEST,
			axum::Json(serde_json::json!({"error": "primary_url is required"})),
		)
			.into_response();
	}

	match services.replication.demote(body.primary_url.clone()).await {
		| Ok(()) => axum::Json(serde_json::json!({
			"status": "demoted",
			"primary_url": body.primary_url,
		}))
		.into_response(),
		| Err(e) => (
			StatusCode::CONFLICT,
			axum::Json(serde_json::json!({"error": e.to_string()})),
		)
			.into_response(),
	}
}

/// Creates a temporary directory that is automatically removed on drop.
///
/// We use a simple wrapper around `std::fs::create_dir_all` on a
/// `tempfile::TempDir` equivalent so we don't add a `tempfile` dependency.
/// Instead, we create a uniquely-named subdirectory in the OS temp dir and
/// delete it ourselves.
fn tempfile_checkpoint_dir() -> Result<TempDir> {
	use std::time::{SystemTime, UNIX_EPOCH};

	let ts = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_nanos();
	let dir =
		std::env::temp_dir().join(format!("tuwunel-checkpoint-{ts}-{}", std::process::id()));
	std::fs::create_dir_all(&dir).map_err(|e| tuwunel_core::err!(Database("{e}")))?;
	Ok(TempDir(dir))
}

struct TempDir(std::path::PathBuf);

impl TempDir {
	fn path(&self) -> &std::path::Path { &self.0 }
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}
