//! Primary-side HTTP handlers for WAL-based RocksDB replication.
//!
//! Endpoints (all protected by `check_replication_token` middleware):
//! - `GET  /_tuwunel/replication/status`        — current sequence number +
//!   role
//! - `GET  /_tuwunel/replication/wal?since=N`   — streaming WAL frame feed
//! - `GET  /_tuwunel/replication/checkpoint`    — full database checkpoint as
//!   tar
//! - `POST /_tuwunel/replication/promote`       — promote secondary to primary
//! - `POST /_tuwunel/replication/demote`        — demote primary back to
//!   secondary

use std::{task::Poll, time::Duration};

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
use tokio::sync::watch;
use tuwunel_core::Result;
use tuwunel_database::{Database, WalFrame, is_wal_gap_error};
use tuwunel_service::replication::drain;

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

	let role = services.replication.role().as_str();

	// For operators and tests: which database this is, whether it is still a
	// proven exact copy of its primary, and whether it holds a handoff for
	// the primary it replaced. See `tuwunel_database::rejoin`.
	let exact_copy_of = services.db.pure_replica_of();
	let has_handoff = matches!(services.db.read_handoff(), Ok(Some(_)));

	Json(serde_json::json!({
		"role": role,
		"latest_sequence": seq,
		"instance": services.db.instance_id(),
		"exact_copy_of": exact_copy_of,
		"has_handoff": has_handoff,
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
	let instance = db.instance_id().to_owned();
	let interval_ms = services
		.server
		.config
		.rocksdb_replication_interval_ms;
	let batch_ms = services
		.server
		.config
		.rocksdb_replication_batch_ms
		.min(interval_ms);

	// Detect a WAL gap before streaming. RocksDB's `GetUpdatesSince(N)` returns
	// frames with seq > N, so a healthy first frame is at `since + 1`. A real
	// gap shows up as an empty iterator (WAL evicted) or a first frame further
	// past `since`. With `manual_wal_flush=true` we flush first so the iterator
	// sees writes still buffered in memory.
	let gap_check: Result<()> = tokio::task::spawn_blocking({
		let db = db.clone();
		move || {
			let _: Result<()> = db.flush_wal();
			let current_seq = db.latest_wal_sequence();
			// A cursor past our latest sequence names writes this database
			// does not have (it lost its unsynced tail in a crash, or it is
			// not the database the cursor came from). Nothing can be resumed
			// from that; waiting for our sequence to catch up to it would
			// splice two different histories together.
			if since > current_seq {
				return Err(tuwunel_core::err!(Database(
					"WAL gap: secondary is at seq {since} but the primary only reaches \
					 {current_seq}; the secondary holds writes this database does not"
				)));
			}
			if since == 0 || since == current_seq {
				return Ok(());
			}
			let mut iter = db.wal_frame_iter(since)?;
			match iter.next() {
				| None => Err(tuwunel_core::err!(Database(
					"WAL gap: frames at seq {since} are not available \
					 (primary at seq {current_seq}); WAL has been compacted"
				))),
				| Some(Err(e)) => Err(e),
				| Some(Ok(frame)) if frame.sequence > since.saturating_add(1) =>
					Err(tuwunel_core::err!(Database(
						"WAL gap: frames in [{since}, {first}) are not available \
						 (primary at seq {current_seq}); WAL has been compacted",
						first = frame.sequence
					))),
				| Some(Ok(_)) => Ok(()),
			}
		}
	})
	.await
	.expect("spawn_blocking panicked in gap check");

	if let Err(ref e) = gap_check {
		if is_wal_gap_error(e) {
			return (StatusCode::GONE, "WAL gap: secondary must re-sync from a fresh checkpoint")
				.into_response();
		}
		return (StatusCode::INTERNAL_SERVER_ERROR, format!("WAL iterator error: {e}"))
			.into_response();
	}

	// Channel that bridges the blocking WAL reader with the async HTTP body.
	let (tx, rx) = futures::channel::mpsc::channel::<Bytes>(256);

	// Registered with the shutdown drain: a clean stop asks this stream to
	// send through the final sequence and end, and waits until it has.
	let (guard, drain) = services.replication.drain().open();

	tokio::spawn(stream_wal(
		move |seq, flush| read_round(&db, seq, flush),
		since,
		tx,
		drain,
		interval_ms,
		batch_ms,
	));

	// The guard lives in the body, so the drain counts this stream as open
	// until the body has been handed over in full (or the connection went
	// away), not merely until its last frame was queued.
	let mut guard = Some(guard);
	let mut rx = rx;
	let stream = futures::stream::poll_fn(move |cx| match rx.poll_next_unpin(cx) {
		| Poll::Ready(None) => {
			if let Some(guard) = guard.take() {
				drop(guard);
			}
			Poll::Ready(None)
		},
		| other => other.map(|item| item.map(Ok::<_, std::convert::Infallible>)),
	});

	Response::builder()
		.status(StatusCode::OK)
		.header(header::CONTENT_TYPE, "application/octet-stream")
		.header("x-tuwunel-role", "primary")
		// Which database this WAL belongs to. The secondary records it with
		// its cursor so that, if it is promoted, its handoff can name the
		// database it replaced (see `rejoin`).
		.header("x-tuwunel-instance-id", instance)
		.body(Body::from_stream(stream))
		.expect("Failed to build WAL streaming response")
}

/// One round of a WAL stream: the encoded frames read, the cursor after them,
/// and why the stream must close, if it must.
type Round = (Vec<Bytes>, u64, Option<String>);

/// Read every WAL batch after `seq` that is available now. `flush` first
/// flushes the in-memory WAL buffer (`manual_wal_flush`), so that a round run
/// for the shutdown drain sees every write up to the final sequence.
///
/// Each frame must be the batch directly after the cursor. The gap check in
/// the handler only runs when the stream opens; a secondary that falls behind
/// far enough *while connected* has the WAL deleted out from under it, and the
/// iterator then quietly starts at the oldest batch it still has. Sending that
/// would leave a hole the secondary cannot see. Stop instead: the secondary
/// reconnects, meets the gap check, and re-syncs from a checkpoint.
fn read_round(db: &Database, seq: u64, flush: bool) -> Round {
	if flush {
		let _: Result<()> = db.flush_wal();
	}

	let mut frames: Vec<Bytes> = Vec::new();
	let mut next_seq = seq;
	let iter = match db.wal_frame_iter(seq) {
		| Ok(iter) => iter,
		| Err(e) => return (frames, next_seq, Some(format!("WAL read: {e}"))),
	};
	for frame in iter {
		let frame = match frame {
			| Ok(frame) => frame,
			| Err(e) => return (frames, next_seq, Some(format!("WAL read: {e}"))),
		};
		if next_seq != 0 && frame.sequence != next_seq.saturating_add(1) {
			return (
				frames,
				next_seq,
				Some(format!(
					"WAL is not contiguous after seq {next_seq}: next batch starts at {}",
					frame.sequence
				)),
			);
		}
		next_seq = frame.resume_seq();
		frames.push(Bytes::from(frame.encode()));
	}
	(frames, next_seq, None)
}

/// The body of one WAL stream: rounds of frames, each followed by a
/// heartbeat, until the secondary disconnects, the WAL breaks, or a clean
/// shutdown's drain asks it to finish.
///
/// On a drain (`drain` becomes `Some(target)`, set only once every writer has
/// stopped) the stream stops pacing, reads through `target`, sends a final
/// heartbeat carrying the sequence it reached — which the secondary saves as
/// its cursor, and records in its handoff if it is promoted — and ends.
async fn stream_wal<R>(
	read: R,
	since: u64,
	mut tx: futures::channel::mpsc::Sender<Bytes>,
	mut drain: watch::Receiver<Option<u64>>,
	interval_ms: u64,
	batch_ms: u64,
) where
	R: Fn(u64, bool) -> Round + Clone + Send + 'static,
{
	let mut seq = since;

	loop {
		// The drain target as this round starts. A round that starts after
		// the drain was requested reads (and flushes) through the final
		// sequence.
		let target = *drain.borrow_and_update();

		// Drain all available WAL frames in a blocking thread.
		let result = tokio::task::spawn_blocking({
			let read = read.clone();
			move || read(seq, target.is_some())
		})
		.await;

		let Ok((frames, next_seq, broken)) = result else {
			return; // spawn_blocking panicked
		};

		let advanced = next_seq != seq;
		seq = next_seq;

		// One network chunk per round, not one per frame. Each item sent
		// on this channel becomes its own HTTP chunk, and the secondary
		// reads the body a chunk at a time — so frames sent singly reach
		// it singly, however many were ready, and it applies each as a
		// separate write. Joined, a round arrives as one read and is
		// applied as one write. The frame encoding is unchanged; a
		// secondary has always decoded as many frames as a read holds.
		for chunk in coalesce(frames, SEND_CHUNK_BYTES) {
			if tx.send(chunk).await.is_err() {
				return; // client disconnected
			}
		}

		if let Some(reason) = broken {
			tuwunel_core::warn!(
				"Closing WAL stream to a secondary at seq {seq}: {reason}. It will reconnect \
				 and re-sync if the WAL it needs is gone."
			);
			return;
		}

		if drain::reached(target, seq) {
			// Everything through the final sequence has been sent. Say where
			// it ends, and close: the primary is stopping.
			let hb = WalFrame::heartbeat(seq);
			if tx.send(Bytes::from(hb.encode())).await.is_ok() {
				tuwunel_core::info!(
					seq,
					"WAL stream to a secondary drained through the final sequence; closing."
				);
			}
			return;
		}

		// Always emit a heartbeat so the secondary can tell the primary is alive.
		// When no data was produced, sleep first to avoid a busy-loop.
		//
		// When data was produced, pause briefly instead of re-reading the
		// WAL at once: under steady writes that turns a stream of one- or
		// two-write rounds into fewer, larger ones (see
		// `rocksdb_replication_batch_ms`). Nothing is skipped by waiting —
		// the next round resumes from `seq` either way.
		//
		// A drain request cuts either pause short. While draining there is
		// no pause after data; and if a draining round found nothing new yet
		// (the WAL does not reach the target — not expected once the writers
		// are stopped and the WAL flushed), it waits out the interval rather
		// than spinning, and the drain's own timeout bounds the whole thing.
		let pause = match (target, advanced) {
			| (_, false) => interval_ms,
			| (None, true) => batch_ms,
			| (Some(_), true) => 0,
		};
		if pause > 0 {
			let sleep = tokio::time::sleep(Duration::from_millis(pause));
			if target.is_some() {
				sleep.await;
			} else {
				tokio::select! {
					() = sleep => {},
					changed = drain.changed() => if changed.is_err() {
						return; // the server is going away
					},
				}
			}
		}

		// The heartbeat carries `seq` — this stream's own cursor, the last
		// sequence it has sent — not the primary's latest sequence. The
		// secondary saves a heartbeat's sequence as its resume cursor, and
		// the latest sequence is read after the drain above, so any write
		// landing in between made the cursor claim batches this stream had
		// not sent: a reconnect after that heartbeat skipped them for good.
		// It is also what a promoted node records as how far it got (see
		// `rejoin::Handoff`), which has to be exact.
		let hb = WalFrame::heartbeat(seq);
		if tx.send(Bytes::from(hb.encode())).await.is_err() {
			return; // client disconnected
		}
	}
}

/// Largest network chunk a round of frames is joined into. A round bigger
/// than this goes out as several chunks; a single frame bigger than this goes
/// out alone, whole.
const SEND_CHUNK_BYTES: usize = 1024 * 1024;

/// Join encoded frames, in order, into chunks of at most `max` bytes each
/// (except that a frame larger than `max` is a chunk by itself). Frames are
/// never split across chunks.
fn coalesce(frames: Vec<Bytes>, max: usize) -> Vec<Bytes> {
	let mut chunks: Vec<Bytes> = Vec::new();
	let mut current: Vec<u8> = Vec::new();

	for frame in frames {
		if !current.is_empty() && current.len().saturating_add(frame.len()) > max {
			chunks.push(Bytes::from(std::mem::take(&mut current)));
		}
		if current.is_empty() && frame.len() >= max {
			chunks.push(frame);
		} else {
			current.extend_from_slice(&frame);
		}
	}
	if !current.is_empty() {
		chunks.push(Bytes::from(current));
	}

	chunks
}

/// `GET /_tuwunel/replication/handoff`
///
/// What this node recorded when it was last promoted: the database it replaced
/// and how far it had replicated from it. A former primary rejoining as this
/// node's replica uses it to prove it lost nothing and skip a full checkpoint
/// (see `tuwunel_database::rejoin`). `404` when there is no record — this node
/// was never promoted, or has been a replica since.
pub(crate) async fn replication_handoff(State(services): State<crate::State>) -> Response {
	match services.db.read_handoff() {
		| Ok(Some(handoff)) => Json(serde_json::json!({
			"from_instance": handoff.from_instance,
			"from_applied_seq": handoff.from_applied_seq,
			"resume_after": handoff.resume_after,
			"instance": handoff.instance,
		}))
		.into_response(),
		| Ok(None) => (StatusCode::NOT_FOUND, "No handoff recorded").into_response(),
		| Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
	}
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
///
/// The archive is streamed, never held in memory. It used to be built into a
/// `Vec<u8>` before the first byte went out, so the primary's resident size
/// grew by the whole database and the OOM
/// killer took the primary down — and a replica retrying its bootstrap every
/// few seconds did it again the moment the primary came back. In flight now:
/// at most `CHECKPOINT_CHANNEL_DEPTH` chunks of `CHECKPOINT_CHUNK_BYTES`.
///
/// One checkpoint at a time: a second request while one is streaming gets
/// `503` with `Retry-After`, so a retry loop cannot stack concurrent archives.
pub(crate) async fn replication_checkpoint(State(services): State<crate::State>) -> Response {
	let Some(guard) = CheckpointGuard::acquire() else {
		return Response::builder()
			.status(StatusCode::SERVICE_UNAVAILABLE)
			.header(header::RETRY_AFTER, "30")
			.body(Body::from("A checkpoint is already being streamed; retry later"))
			.expect("Failed to build checkpoint busy response");
	};

	let db = services.db.clone();
	let instance = db.instance_id().to_owned();

	// Alongside the database, not in the OS temp dir: a RocksDB checkpoint is
	// hard links only when it is on the same filesystem as the database. On a
	// different one (a tmpfs /tmp, say) it copies every SST file — onto RAM,
	// in the tmpfs case, which is the failure this handler exists to avoid.
	let db_path = services.server.config.database_path.clone();
	let checkpoint_parent = db_path.parent().unwrap_or(&db_path).to_path_buf();

	let (seq_tx, seq_rx) = tokio::sync::oneshot::channel::<Result<u64>>();
	let (chunk_tx, mut chunk_rx) =
		tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(CHECKPOINT_CHANNEL_DEPTH);

	// Checkpoint creation and tar writing are blocking I/O. The thread owns the
	// temp dir and the busy guard, so both are released when the stream ends —
	// including when the secondary disconnects partway, which surfaces here as
	// a failed send and aborts the archive.
	tokio::task::spawn_blocking(move || {
		let _guard = guard;

		// Holding the guard, so any checkpoint dir already here was left by a
		// process that died mid-stream (SIGKILL skips TempDir's drop). Its hard
		// links pin SST files compaction has since deleted, so it grows into a
		// second copy of the database on disk if left alone.
		remove_stale_checkpoint_dirs(&checkpoint_parent);

		let tmp = match tempfile_checkpoint_dir(&checkpoint_parent) {
			| Ok(tmp) => tmp,
			| Err(e) => {
				let _ = seq_tx.send(Err(e));
				return;
			},
		};
		let checkpoint_path = tmp.path().join("checkpoint");

		match db.create_checkpoint(&checkpoint_path) {
			| Ok(seq) =>
				if seq_tx.send(Ok(seq)).is_err() {
					return; // handler went away before headers were sent
				},
			| Err(e) => {
				let _ = seq_tx.send(Err(e));
				return;
			},
		}

		let writer = std::io::BufWriter::with_capacity(
			CHECKPOINT_CHUNK_BYTES,
			ChannelWriter(chunk_tx.clone()),
		);
		let mut builder = tar::Builder::new(writer);
		let written = builder
			.append_dir_all("checkpoint", &checkpoint_path)
			.and_then(|()| builder.into_inner())
			.and_then(|mut w| std::io::Write::flush(&mut w));

		if let Err(e) = written {
			// Ends the body with an error, so the secondary sees a failed
			// transfer rather than a short archive that parses as complete.
			let _ = chunk_tx.blocking_send(Err(e));
		}
	});

	let seq = match seq_rx.await {
		| Ok(Ok(seq)) => seq,
		| Ok(Err(e)) =>
			return (StatusCode::INTERNAL_SERVER_ERROR, format!("Checkpoint creation failed: {e}"))
				.into_response(),
		| Err(_) =>
			return (StatusCode::INTERNAL_SERVER_ERROR, "Checkpoint task ended unexpectedly")
				.into_response(),
	};

	let stream = futures::stream::poll_fn(move |cx| chunk_rx.poll_recv(cx));

	Response::builder()
		.status(StatusCode::OK)
		.header(header::CONTENT_TYPE, "application/x-tar")
		.header("x-tuwunel-checkpoint-sequence", seq.to_string())
		// Which database this checkpoint is a copy of; see `rejoin::Purity`.
		.header("x-tuwunel-instance-id", instance)
		.body(Body::from_stream(stream))
		.expect("Failed to build checkpoint response")
}

/// Size of each streamed checkpoint chunk.
const CHECKPOINT_CHUNK_BYTES: usize = 1024 * 1024;

/// Chunks buffered between the tar writer and the socket. Bounds the memory
/// a checkpoint can hold at `CHECKPOINT_CHUNK_BYTES * CHECKPOINT_CHANNEL_DEPTH`.
const CHECKPOINT_CHANNEL_DEPTH: usize = 8;

static CHECKPOINT_IN_PROGRESS: std::sync::atomic::AtomicBool =
	std::sync::atomic::AtomicBool::new(false);

/// Held for the life of one checkpoint stream; releases the slot on drop.
struct CheckpointGuard;

impl CheckpointGuard {
	fn acquire() -> Option<Self> {
		use std::sync::atomic::Ordering;

		CHECKPOINT_IN_PROGRESS
			.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
			.ok()
			.map(|_| Self)
	}
}

impl Drop for CheckpointGuard {
	fn drop(&mut self) {
		CHECKPOINT_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
	}
}

/// `Write` adapter that hands each buffer to the response body. Blocks when the
/// channel is full, which is what holds the tar writer to the socket's pace.
struct ChannelWriter(tokio::sync::mpsc::Sender<std::io::Result<Bytes>>);

impl std::io::Write for ChannelWriter {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		self.0
			.blocking_send(Ok(Bytes::copy_from_slice(buf)))
			.map_err(|_| {
				std::io::Error::new(std::io::ErrorKind::BrokenPipe, "checkpoint receiver closed")
			})?;
		Ok(buf.len())
	}

	fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
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
/// - `409 Conflict` if this instance is already a primary (no
///   `rocksdb_primary_url` was configured, or it was already promoted).
pub(crate) async fn replication_promote(
	State(services): State<crate::State>,
) -> impl IntoResponse {
	if services.replication.is_promoted() {
		return (StatusCode::CONFLICT, Json(serde_json::json!({"error": "already promoted"})))
			.into_response();
	}

	if services
		.server
		.config
		.rocksdb_primary_url
		.is_none()
	{
		return (
			StatusCode::CONFLICT,
			Json(
				serde_json::json!({"error": "not a secondary; no rocksdb_primary_url configured"}),
			),
		)
			.into_response();
	}

	if let Err(e) = services.replication.promote() {
		return (
			StatusCode::INTERNAL_SERVER_ERROR,
			Json(serde_json::json!({"error": format!("promote failed: {e}")})),
		)
			.into_response();
	}

	Json(serde_json::json!({"status": "promoted"})).into_response()
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
			Json(serde_json::json!({"error": "primary_url is required"})),
		)
			.into_response();
	}

	match services
		.replication
		.demote(body.primary_url.clone())
		.await
	{
		| Ok(()) => Json(serde_json::json!({
			"status": "demoted",
			"primary_url": body.primary_url,
		}))
		.into_response(),
		| Err(e) => (StatusCode::CONFLICT, Json(serde_json::json!({"error": e.to_string()})))
			.into_response(),
	}
}

/// Creates a uniquely-named directory under `parent` that is removed on drop.
///
/// `parent` must be on the database's filesystem (see `replication_checkpoint`).
/// A plain wrapper rather than the `tempfile` crate, to avoid the dependency.
fn tempfile_checkpoint_dir(parent: &std::path::Path) -> Result<TempDir> {
	use std::time::{SystemTime, UNIX_EPOCH};

	let ts = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_nanos();
	let dir = parent.join(format!("{CHECKPOINT_DIR_PREFIX}{ts}-{}", std::process::id()));
	std::fs::create_dir_all(&dir).map_err(|e| tuwunel_core::err!(Database("{e}")))?;
	Ok(TempDir(dir))
}

const CHECKPOINT_DIR_PREFIX: &str = "_replication_checkpoint-";

fn remove_stale_checkpoint_dirs(parent: &std::path::Path) {
	let Ok(entries) = std::fs::read_dir(parent) else {
		return;
	};

	for entry in entries.flatten() {
		if entry
			.file_name()
			.to_str()
			.is_some_and(|name| name.starts_with(CHECKPOINT_DIR_PREFIX))
		{
			let _: std::io::Result<()> = std::fs::remove_dir_all(entry.path());
		}
	}
}

struct TempDir(std::path::PathBuf);

impl TempDir {
	fn path(&self) -> &std::path::Path { &self.0 }
}

impl Drop for TempDir {
	fn drop(&mut self) { let _: std::io::Result<()> = std::fs::remove_dir_all(&self.0); }
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A tree tarred through `ChannelWriter` arrives in bounded chunks and
	/// unpacks byte-for-byte, and the writer never buffers the whole archive.
	#[test]
	fn checkpoint_tar_streams_in_bounded_chunks() {
		let src = tempfile_checkpoint_dir(&std::env::temp_dir()).unwrap();
		let tree = src.path().join("checkpoint");
		std::fs::create_dir_all(tree.join("sub")).unwrap();
		let big: Vec<u8> = (0..(3 * CHECKPOINT_CHUNK_BYTES + 12345))
			.map(|i| (i % 251) as u8)
			.collect();
		std::fs::write(tree.join("000001.sst"), &big).unwrap();
		std::fs::write(tree.join("sub/CURRENT"), b"MANIFEST-000001\n").unwrap();

		let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(2);
		let writer_tree = tree.clone();
		let writer = std::thread::spawn(move || {
			let w = std::io::BufWriter::with_capacity(CHECKPOINT_CHUNK_BYTES, ChannelWriter(tx));
			let mut builder = tar::Builder::new(w);
			builder.append_dir_all("checkpoint", &writer_tree).unwrap();
			let mut w = builder.into_inner().unwrap();
			std::io::Write::flush(&mut w).unwrap();
		});

		let mut archive = Vec::new();
		while let Some(chunk) = rx.blocking_recv() {
			let chunk = chunk.unwrap();
			assert!(chunk.len() <= CHECKPOINT_CHUNK_BYTES, "chunk of {} bytes", chunk.len());
			archive.extend_from_slice(&chunk);
		}
		writer.join().unwrap();

		let out = tempfile_checkpoint_dir(&std::env::temp_dir()).unwrap();
		tar::Archive::new(&archive[..]).unpack(out.path()).unwrap();
		assert_eq!(std::fs::read(out.path().join("checkpoint/000001.sst")).unwrap(), big);
		assert_eq!(
			std::fs::read(out.path().join("checkpoint/sub/CURRENT")).unwrap(),
			b"MANIFEST-000001\n"
		);
	}

	/// A receiver that goes away (secondary disconnected) fails the writer
	/// instead of blocking it forever.
	#[test]
	fn checkpoint_writer_fails_when_receiver_drops() {
		let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(1);
		drop(rx);
		let err = std::io::Write::write(&mut ChannelWriter(tx), b"x").unwrap_err();
		assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
	}

	#[test]
	fn coalesce_joins_frames_in_order_without_splitting_any() {
		let frame = |n: u8, len: usize| Bytes::from(vec![n; len]);
		let frames = vec![frame(1, 10), frame(2, 10), frame(3, 15), frame(4, 40), frame(5, 5)];
		let all: Vec<u8> = frames.iter().flat_map(|f| f.to_vec()).collect();

		let chunks = coalesce(frames, 30);
		let sizes: Vec<usize> = chunks.iter().map(Bytes::len).collect();
		// 10+10 fit; 15 would pass 30 so starts a new chunk; 40 is larger
		// than the limit and travels alone; 5 follows it.
		assert_eq!(sizes, vec![20, 15, 40, 5]);
		let joined: Vec<u8> = chunks.iter().flat_map(|c| c.to_vec()).collect();
		assert_eq!(joined, all, "same bytes, same order");

		assert!(coalesce(Vec::new(), 30).is_empty());
	}

	/// What the secondary sees: a joined round decodes back into the same
	/// frames, one after another.
	#[test]
	fn coalesced_round_decodes_into_the_original_frames() {
		let originals = vec![
			WalFrame::data(100, 1, b"one".to_vec()),
			WalFrame::data(101, 3, b"three ops".to_vec()),
			WalFrame::data(104, 1, b"last".to_vec()),
		];
		let encoded: Vec<Bytes> = originals.iter().map(|f| Bytes::from(f.encode())).collect();
		let chunks = coalesce(encoded, SEND_CHUNK_BYTES);
		assert_eq!(chunks.len(), 1, "a small round is one chunk");

		let buf = &chunks[0];
		let mut offset = 0;
		let mut decoded = Vec::new();
		while let Ok((frame, used)) = WalFrame::decode(&buf[offset..]) {
			decoded.push(frame);
			offset += used;
		}
		assert_eq!(offset, buf.len());
		assert_eq!(decoded.len(), 3);
		for (got, want) in decoded.iter().zip(&originals) {
			assert_eq!(got.sequence, want.sequence);
			assert_eq!(got.batch_data, want.batch_data);
		}
	}

	/// A WAL for `stream_wal` tests: batches `(sequence, count)`, and how
	/// often a round asked for a flush.
	#[derive(Clone, Default)]
	struct FakeWal {
		batches: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>>,
		flushes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
	}

	impl FakeWal {
		fn push(&self, sequence: u64, count: u64) {
			self.batches.lock().unwrap().push((sequence, count));
		}

		fn reader(&self) -> impl Fn(u64, bool) -> Round + Clone + Send + 'static {
			let wal = self.clone();
			move |seq, flush| {
				if flush {
					wal.flushes
						.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
				}
				let mut next = seq;
				let mut frames = Vec::new();
				for &(sequence, count) in wal.batches.lock().unwrap().iter() {
					if sequence > next {
						let frame = WalFrame::data(sequence, count, vec![1, 2, 3]);
						next = frame.resume_seq();
						frames.push(Bytes::from(frame.encode()));
					}
				}
				(frames, next, None)
			}
		}
	}

	/// Decode whatever arrives within `wait` (one overall deadline, not per
	/// chunk: heartbeats keep arriving); `true` when the body ended.
	async fn collect(
		rx: &mut futures::channel::mpsc::Receiver<Bytes>,
		wait: Duration,
	) -> (Vec<WalFrame>, bool) {
		let deadline = tokio::time::Instant::now()
			.checked_add(wait)
			.unwrap();
		let mut buf = Vec::new();
		let ended = loop {
			match tokio::time::timeout_at(deadline, rx.next()).await {
				| Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
				| Ok(None) => break true,
				| Err(_) => break false,
			}
		};
		let mut frames = Vec::new();
		let mut offset = 0;
		while offset < buf.len() {
			let (frame, used) = WalFrame::decode(&buf[offset..]).unwrap();
			frames.push(frame);
			offset = offset.saturating_add(used);
		}
		(frames, ended)
	}

	fn data_seqs(frames: &[WalFrame]) -> Vec<u64> {
		frames
			.iter()
			.filter(|f| f.frame_type == tuwunel_database::FRAME_TYPE_DATA)
			.map(|f| f.sequence)
			.collect()
	}

	/// The shutdown case: an idle stream (long poll interval) wakes as soon
	/// as the drain is requested, sends the writes made while stopping,
	/// flushes before reading, ends with a heartbeat at the final sequence,
	/// and closes — well before its interval would have elapsed.
	#[tokio::test]
	async fn drain_sends_final_writes_then_closes() {
		let wal = FakeWal::default();
		wal.push(11, 1);
		wal.push(12, 2);
		wal.push(14, 1);

		let (tx, mut rx) = futures::channel::mpsc::channel::<Bytes>(256);
		let (target_tx, target_rx) = watch::channel(None);
		let stream = tokio::spawn(stream_wal(wal.reader(), 10, tx, target_rx, 60_000, 0));

		let (frames, ended) = collect(&mut rx, Duration::from_millis(300)).await;
		assert!(!ended);
		assert_eq!(data_seqs(&frames), vec![11, 12, 14]);
		assert_eq!(frames.last().unwrap().sequence, 14, "heartbeat carries the cursor");
		assert_eq!(wal.flushes.load(std::sync::atomic::Ordering::SeqCst), 0);

		// What the stopping services wrote, then the drain request.
		wal.push(15, 1);
		wal.push(16, 3);
		let start = std::time::Instant::now();
		target_tx.send_replace(Some(18));

		let (frames, ended) = collect(&mut rx, Duration::from_secs(5)).await;
		assert!(ended, "stream must close after the drain");
		assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
		assert_eq!(data_seqs(&frames), vec![15, 16]);
		let last = frames.last().unwrap();
		assert_eq!(last.frame_type, tuwunel_database::FRAME_TYPE_HEARTBEAT);
		assert_eq!(last.sequence, 18, "final heartbeat is the final sequence");
		assert!(wal.flushes.load(std::sync::atomic::Ordering::SeqCst) >= 1);
		stream.await.unwrap();
	}

	/// A stream opened after the drain began (or that reaches the target in
	/// its first round) sends what is left, the final heartbeat, and closes
	/// without another round or pause.
	#[tokio::test]
	async fn drain_target_already_sent_closes_at_once() {
		let wal = FakeWal::default();
		wal.push(21, 1);

		let (tx, mut rx) = futures::channel::mpsc::channel::<Bytes>(256);
		let (_target_tx, target_rx) = watch::channel(Some(21));
		let stream = tokio::spawn(stream_wal(wal.reader(), 20, tx, target_rx, 60_000, 25));

		let (frames, ended) = collect(&mut rx, Duration::from_secs(5)).await;
		assert!(ended);
		assert_eq!(data_seqs(&frames), vec![21]);
		assert_eq!(frames.len(), 2, "data, then the final heartbeat; no extra round");
		assert_eq!(frames[1].frame_type, tuwunel_database::FRAME_TYPE_HEARTBEAT);
		assert_eq!(frames[1].sequence, 21);
		stream.await.unwrap();
	}

	/// The drain waits for writes it was told about: a target past what the
	/// WAL holds keeps the stream open (paced, not spinning) until they show
	/// up. The drain's own timeout bounds this in a real shutdown.
	#[tokio::test]
	async fn drain_waits_until_the_target_is_in_the_wal() {
		let wal = FakeWal::default();

		let (tx, mut rx) = futures::channel::mpsc::channel::<Bytes>(256);
		let (_target_tx, target_rx) = watch::channel(Some(31));
		let stream = tokio::spawn(stream_wal(wal.reader(), 30, tx, target_rx, 50, 0));

		let (frames, ended) = collect(&mut rx, Duration::from_millis(200)).await;
		assert!(!ended);
		assert!(data_seqs(&frames).is_empty());
		let rounds = wal.flushes.load(std::sync::atomic::Ordering::SeqCst);
		assert!(rounds < 20, "paced while waiting, not spinning: {rounds} rounds");

		wal.push(31, 1);
		let (frames, ended) = collect(&mut rx, Duration::from_secs(5)).await;
		assert!(ended);
		assert_eq!(data_seqs(&frames), vec![31]);
		assert_eq!(frames.last().unwrap().sequence, 31);
		stream.await.unwrap();
	}

	/// No drain: the stream is unchanged — it keeps sending heartbeats and
	/// never closes on its own.
	#[tokio::test]
	async fn without_drain_stream_stays_open() {
		let wal = FakeWal::default();
		wal.push(41, 1);

		let (tx, mut rx) = futures::channel::mpsc::channel::<Bytes>(256);
		let (_target_tx, target_rx) = watch::channel(None);
		let stream = tokio::spawn(stream_wal(wal.reader(), 40, tx, target_rx, 20, 0));

		let (frames, ended) = collect(&mut rx, Duration::from_millis(300)).await;
		assert!(!ended);
		assert_eq!(data_seqs(&frames), vec![41]);
		let heartbeats = frames
			.iter()
			.filter(|f| f.frame_type == tuwunel_database::FRAME_TYPE_HEARTBEAT)
			.count();
		assert!(heartbeats >= 2, "{heartbeats}");
		assert_eq!(wal.flushes.load(std::sync::atomic::Ordering::SeqCst), 0);

		drop(rx);
		tokio::time::timeout(Duration::from_secs(2), stream)
			.await
			.expect("stream ends once the secondary disconnects")
			.unwrap();
	}

	#[test]
	fn checkpoint_guard_allows_one_at_a_time() {
		let first = CheckpointGuard::acquire().expect("first acquire");
		assert!(CheckpointGuard::acquire().is_none());
		drop(first);
		assert!(CheckpointGuard::acquire().is_some());
	}

	#[test]
	fn stale_checkpoint_dirs_are_removed() {
		let parent = tempfile_checkpoint_dir(&std::env::temp_dir()).unwrap();
		let stale = parent.path().join(format!("{CHECKPOINT_DIR_PREFIX}1-1"));
		std::fs::create_dir_all(stale.join("checkpoint")).unwrap();
		let keep = parent.path().join("data");
		std::fs::create_dir_all(&keep).unwrap();

		remove_stale_checkpoint_dirs(parent.path());

		assert!(!stale.exists());
		assert!(keep.exists());
	}
}
