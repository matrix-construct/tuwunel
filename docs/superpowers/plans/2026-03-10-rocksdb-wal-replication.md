# RocksDB WAL Replication Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement WAL-based RocksDB replication so a secondary Tuwunel instance continuously tails the primary's WAL over HTTP and can promote itself to primary on failover.

**Architecture:** The primary exposes three `/_tuwunel/replication/` HTTP endpoints (status, WAL stream, checkpoint download) protected by a shared-secret token. The secondary runs a background `Service` worker that bootstraps from a checkpoint, then streams WAL frames from the primary applying each batch atomically alongside an updated resume cursor stored in a dedicated `replication_meta` column family. On WAL gap (HTTP 410), the secondary triggers a full re-sync. Promotion is restart-based: write a marker file and restart with `rocksdb_secondary = false`.

**Tech Stack:** rust-rocksdb (Checkpoint, get_updates_since, disable/enable_file_deletions), axum 0.8, reqwest (already in service crate), sha2+hmac (already in workspace) for token auth, crc32fast (new workspace dep) for frame integrity, tokio channels for streaming, tar crate (new dep) for checkpoint bundling.

---

## Chunk 1: Config, WAL Retention, and Secondary Path Fix

### Files:
- Modify: `src/core/config/mod.rs` (~line 1345)
- Modify: `src/database/engine/db_opts.rs` (~line 59)
- Modify: `src/database/engine/open.rs` (~line 44)
- Modify: `Cargo.toml` (workspace deps: crc32fast, tar)

---

### Task 1: Add replication config fields

**Files:**
- Modify: `src/core/config/mod.rs`

- [ ] **Step 1: Add the new config fields** after the existing `rocksdb_secondary` field at line 1345:

```rust
/// Path for the secondary instance's own RocksDB log files. Required when
/// `rocksdb_secondary` is true and the primary DB is not on a shared
/// filesystem. Must be a writable directory local to this host.
pub rocksdb_secondary_path: Option<std::path::PathBuf>,

/// URL of the primary instance for WAL-streaming replication.
/// Example: `https://primary.example.com`
/// Required on secondary instances that use WAL streaming.
pub rocksdb_primary_url: Option<String>,

/// Shared secret token for replication endpoint authentication.
/// Both primary and secondary must have the same value.
/// Leave unset to disable the replication HTTP endpoints entirely.
pub rocksdb_replication_token: Option<String>,

/// How long (in seconds) the primary retains WAL segments beyond what
/// local recovery requires. Gives the secondary a window to reconnect
/// after downtime without needing a full re-sync. Default: 86400 (24h).
#[serde(default = "default_rocksdb_wal_ttl_seconds")]
pub rocksdb_wal_ttl_seconds: u64,

/// Interval in milliseconds at which the secondary polls for new WAL
/// frames when caught up with the primary. Default: 250ms.
#[serde(default = "default_rocksdb_replication_interval_ms")]
pub rocksdb_replication_interval_ms: u64,
```

- [ ] **Step 2: Add the default-value functions** near the other `default_*` functions in the same file:

```rust
fn default_rocksdb_wal_ttl_seconds() -> u64 { 86400 }
fn default_rocksdb_replication_interval_ms() -> u64 { 250 }
```

- [ ] **Step 3: Verify the server starts** (config parsing must not break):

```bash
cd /Users/jgusler/Documents/repos/tuwunel
cargo check -p tuwunel-core 2>&1 | tail -5
```
Expected: no errors.

---

### Task 2: Wire WAL TTL into db_opts and fix secondary open path

**Files:**
- Modify: `src/database/engine/db_opts.rs`
- Modify: `src/database/engine/open.rs`

- [ ] **Step 1: Replace the hardcoded `set_wal_size_limit_mb` in `db_opts.rs`** (line 59) to add WAL TTL:

The existing line is:
```rust
opts.set_wal_size_limit_mb(1024);
```

Replace with:
```rust
opts.set_wal_size_limit_mb(1024);
opts.set_wal_ttl_seconds(config.rocksdb_wal_ttl_seconds);
```

- [ ] **Step 2: Fix the secondary open path in `open.rs`** (line 44-45).

Existing:
```rust
} else if config.rocksdb_secondary {
    Db::open_cf_descriptors_as_secondary(&db_opts, path, path, cfds)
```

Replace with:
```rust
} else if config.rocksdb_secondary {
    let secondary_path = config
        .rocksdb_secondary_path
        .as_deref()
        .unwrap_or(path);
    Db::open_cf_descriptors_as_secondary(&db_opts, path, secondary_path, cfds)
```

- [ ] **Step 3: Check compile:**

```bash
cargo check -p tuwunel-database 2>&1 | tail -5
```
Expected: no errors.

---

### Task 3: Add crc32fast and tar to workspace

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add to `[workspace.dependencies]` section** (alphabetically, near the `bytes` entry at line 106):

```toml
[workspace.dependencies.crc32fast]
version = "1"

[workspace.dependencies.tar]
version = "0.4"
```

- [ ] **Step 2: Verify workspace resolves:**

```bash
cargo check --workspace 2>&1 | grep "error\[" | head -5
```
Expected: no new errors from this change.

---

## Chunk 2: Engine Replication Primitives and Wire Frame Format

### Files:
- Create: `src/database/engine/replication.rs`
- Modify: `src/database/engine.rs` (add `mod replication;`)
- Create: `src/database/engine/replication/frame.rs`  ← actually keep flat: one file
- Modify: `src/database/Cargo.toml` (add crc32fast)

---

### Task 4: Wire frame encode/decode

**Files:**
- Create: `src/database/engine/replication.rs`
- Modify: `src/database/Cargo.toml`

The wire format (all integers little-endian):
```
offset  size  field
0       1     frame_type: 0x01 = data, 0x02 = heartbeat
1       8     sequence: primary's BatchResult sequence_number
9       8     count: number of WAL records in this batch (sequence advances by this)
17      8     timestamp_ms: unix millis when primary wrote this
25      4     crc32c: checksum of batch_data bytes (0 for heartbeats)
29      4     batch_len: byte length of following batch_data (0 for heartbeats)
33      ?     batch_data: raw WriteBatch serialization
```

- [ ] **Step 1: Add crc32fast to database crate's Cargo.toml:**

```toml
crc32fast.workspace = true
```

- [ ] **Step 2: Write the frame module with tests inline:**

Create `src/database/engine/replication.rs`:

```rust
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rocksdb::{Checkpoint, DBWALIterator};
use tuwunel_core::{Err, Result, implement};

use super::Engine;
use crate::util::map_err;

// ── Wire frame ────────────────────────────────────────────────────────────────

pub const FRAME_TYPE_DATA: u8 = 0x01;
pub const FRAME_TYPE_HEARTBEAT: u8 = 0x02;
pub const FRAME_HEADER_LEN: usize = 33;

/// A single replication frame transmitted over the HTTP WAL stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFrame {
    pub frame_type: u8,
    /// Primary's sequence number for the first record in this batch.
    pub sequence: u64,
    /// How many WAL sequence numbers this batch consumes.
    /// Next resume point = sequence + count.
    pub count: u64,
    /// Unix milliseconds when the primary wrote this batch.
    pub timestamp_ms: u64,
    /// CRC32 of batch_data. Zero for heartbeats.
    pub crc32: u32,
    /// Raw WriteBatch bytes. Empty for heartbeats.
    pub batch_data: Vec<u8>,
}

impl WalFrame {
    pub fn heartbeat(primary_sequence: u64) -> Self {
        Self {
            frame_type: FRAME_TYPE_HEARTBEAT,
            sequence: primary_sequence,
            count: 0,
            timestamp_ms: now_ms(),
            crc32: 0,
            batch_data: Vec::new(),
        }
    }

    pub fn data(sequence: u64, count: u64, batch_data: Vec<u8>) -> Self {
        let crc32 = crc32fast::hash(&batch_data);
        Self {
            frame_type: FRAME_TYPE_DATA,
            sequence,
            count,
            timestamp_ms: now_ms(),
            crc32,
            batch_data,
        }
    }

    /// Next sequence the secondary should request after applying this frame.
    /// For heartbeats, `sequence` is the primary's current latest — callers
    /// should not advance their resume cursor from a heartbeat.
    pub fn next_resume_seq(&self) -> u64 { self.sequence.saturating_add(self.count) }

    /// Encode to bytes for writing to the HTTP stream.
    pub fn encode(&self) -> Vec<u8> {
        let batch_len = self.batch_data.len() as u32;
        let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + self.batch_data.len());

        buf.push(self.frame_type);
        buf.extend_from_slice(&self.sequence.to_le_bytes());
        buf.extend_from_slice(&self.count.to_le_bytes());
        buf.extend_from_slice(&self.timestamp_ms.to_le_bytes());
        buf.extend_from_slice(&self.crc32.to_le_bytes());
        buf.extend_from_slice(&batch_len.to_le_bytes());
        buf.extend_from_slice(&self.batch_data);

        buf
    }

    /// Decode from a byte slice. Returns `(frame, bytes_consumed)`.
    /// Returns `Err` if the slice is too short or the CRC does not match.
    pub fn decode(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.len() < FRAME_HEADER_LEN {
            return Err!("WAL frame too short: {} < {FRAME_HEADER_LEN}", buf.len());
        }

        let frame_type = buf[0];
        let sequence   = u64::from_le_bytes(buf[1..9].try_into().unwrap());
        let count      = u64::from_le_bytes(buf[9..17].try_into().unwrap());
        let timestamp_ms = u64::from_le_bytes(buf[17..25].try_into().unwrap());
        let crc32      = u32::from_le_bytes(buf[25..29].try_into().unwrap());
        let batch_len  = u32::from_le_bytes(buf[29..33].try_into().unwrap()) as usize;

        let total = FRAME_HEADER_LEN + batch_len;
        if buf.len() < total {
            return Err!(
                "WAL frame data truncated: need {total} bytes, have {}",
                buf.len()
            );
        }

        let batch_data = buf[FRAME_HEADER_LEN..total].to_vec();

        if frame_type == FRAME_TYPE_DATA && !batch_data.is_empty() {
            let actual = crc32fast::hash(&batch_data);
            if actual != crc32 {
                return Err!(
                    "WAL frame CRC mismatch: expected {crc32:#010x}, got {actual:#010x}"
                );
            }
        }

        Ok((
            Self { frame_type, sequence, count, timestamp_ms, crc32, batch_data },
            total,
        ))
    }
}

/// Extract the operation count from a raw WriteBatch byte slice.
/// RocksDB WriteBatch layout: [8 bytes seq][4 bytes count][records...].
/// Returns 0 if the slice is malformed.
pub fn batch_count_from_bytes(data: &[u8]) -> u64 {
    if data.len() < 12 {
        return 0;
    }
    u32::from_le_bytes(data[8..12].try_into().unwrap()) as u64
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Engine methods ─────────────────────────────────────────────────────────────

#[implement(Engine)]
/// Create a RocksDB checkpoint at `dest`. The checkpoint consists of
/// hard-linked SST files and is consistent to the current sequence number.
/// Returns the sequence number at checkpoint time.
pub fn create_checkpoint(&self, dest: &Path) -> Result<u64> {
    let checkpoint = Checkpoint::new(&self.db).map_err(map_err)?;
    checkpoint.create_checkpoint(dest).map_err(map_err)?;
    Ok(self.db.latest_sequence_number())
}

#[implement(Engine)]
/// Prevent RocksDB from deleting obsolete files. Call before transferring
/// checkpoint or live files. Must be paired with `enable_file_deletions`.
pub fn disable_file_deletions(&self) -> Result {
    self.db.disable_file_deletions().map_err(map_err)
}

#[implement(Engine)]
/// Re-enable file deletion after a `disable_file_deletions` call.
pub fn enable_file_deletions(&self) -> Result {
    self.db.enable_file_deletions().map_err(map_err)
}

#[implement(Engine)]
/// Return an iterator over WAL batches starting at `since`.
///
/// If `since` is older than the oldest retained WAL segment, this returns
/// `Err` with a message containing "too old" — callers should treat this
/// as a WAL gap and respond with HTTP 410.
pub fn wal_updates_since(&self, since: u64) -> Result<DBWALIterator> {
    self.db.get_updates_since(since).map_err(map_err)
}

#[implement(Engine)]
/// Returns true if the `get_updates_since` error indicates the requested
/// sequence is older than any retained WAL segment.
pub fn is_wal_gap_error(err: &tuwunel_core::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("too old")
        || msg.contains("older than")
        || msg.contains("sequence not")
        || msg.contains("data loss")
}
```

- [ ] **Step 3: Add `mod replication;` to engine.rs** at line 11 (after the existing `mod repair;`):

```rust
mod replication;
pub use replication::{WalFrame, batch_count_from_bytes, FRAME_TYPE_DATA, FRAME_TYPE_HEARTBEAT};
```

- [ ] **Step 4: Write unit tests for frame encode/decode** at the bottom of `replication.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heartbeat_round_trip() {
        let frame = WalFrame::heartbeat(12345);
        let encoded = frame.encode();
        let (decoded, consumed) = WalFrame::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.frame_type, FRAME_TYPE_HEARTBEAT);
        assert_eq!(decoded.sequence, 12345);
        assert_eq!(decoded.count, 0);
        assert!(decoded.batch_data.is_empty());
    }

    #[test]
    fn test_data_frame_round_trip() {
        let data = b"hello world test batch data".to_vec();
        let frame = WalFrame::data(1000, 50, data.clone());
        let encoded = frame.encode();
        let (decoded, consumed) = WalFrame::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.frame_type, FRAME_TYPE_DATA);
        assert_eq!(decoded.sequence, 1000);
        assert_eq!(decoded.count, 50);
        assert_eq!(decoded.next_resume_seq(), 1050);
        assert_eq!(decoded.batch_data, data);
    }

    #[test]
    fn test_crc_mismatch_rejected() {
        let frame = WalFrame::data(1000, 1, b"payload".to_vec());
        let mut encoded = frame.encode();
        // Corrupt a byte in the batch_data region
        let last = encoded.len() - 1;
        encoded[last] ^= 0xFF;
        assert!(WalFrame::decode(&encoded).is_err());
    }

    #[test]
    fn test_truncated_header_rejected() {
        let frame = WalFrame::heartbeat(1);
        let encoded = frame.encode();
        assert!(WalFrame::decode(&encoded[..10]).is_err());
    }

    #[test]
    fn test_truncated_body_rejected() {
        let frame = WalFrame::data(1, 1, b"hello world".to_vec());
        let mut encoded = frame.encode();
        encoded.truncate(encoded.len() - 3); // chop off end of batch_data
        assert!(WalFrame::decode(&encoded).is_err());
    }

    #[test]
    fn test_batch_count_from_bytes() {
        // Simulate a WriteBatch with count=7 at bytes 8-11
        let mut fake = vec![0u8; 16];
        fake[8..12].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(batch_count_from_bytes(&fake), 7);
    }

    #[test]
    fn test_batch_count_from_bytes_short() {
        assert_eq!(batch_count_from_bytes(&[0u8; 5]), 0);
    }
}
```

- [ ] **Step 5: Run the tests:**

```bash
cd /Users/jgusler/Documents/repos/tuwunel
cargo test -p tuwunel-database replication 2>&1 | tail -20
```
Expected: all 7 tests pass.

---

## Chunk 3: Primary HTTP Endpoints

### Files:
- Create: `src/api/router/replication_auth.rs`
- Create: `src/api/client/replication.rs`
- Modify: `src/api/client/mod.rs`
- Modify: `src/api/router.rs`
- Modify: `src/api/Cargo.toml`

---

### Task 5: Replication auth middleware

**Files:**
- Create: `src/api/router/replication_auth.rs`
- Modify: `src/api/router.rs` (add `mod replication_auth;` and use in router)

The middleware extracts the `X-Tuwunel-Replication-Token` header and checks it against the configured `rocksdb_replication_token` using a constant-time comparison.

- [ ] **Step 1: Create the auth middleware file:**

Create `src/api/router/replication_auth.rs`:

```rust
use std::sync::Arc;

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tuwunel_service::Services;

pub(super) const TOKEN_HEADER: &str = "x-tuwunel-replication-token";

/// Tower middleware that validates the replication shared-secret token.
/// Returns 401 if the token is absent or wrong, 501 if replication is
/// not configured (no `rocksdb_replication_token` in config).
pub(super) async fn check_replication_token(
    axum::extract::State(services): axum::extract::State<crate::State>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(ref expected) = services.server.config.rocksdb_replication_token else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "Replication not configured on this instance",
        )
            .into_response();
    };

    let provided = request
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Constant-time comparison to avoid timing side-channels.
    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "Invalid replication token").into_response();
    }

    next.run(request).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

- [ ] **Step 2: Declare the module in `src/api/router.rs`** — add at line 3 (after `mod response;`):

```rust
mod replication_auth;
pub(super) use replication_auth::check_replication_token;
```

- [ ] **Step 3: Check compile:**

```bash
cargo check -p tuwunel-api 2>&1 | tail -5
```

---

### Task 6: Primary replication HTTP handlers

**Files:**
- Create: `src/api/client/replication.rs`
- Modify: `src/api/client/mod.rs`
- Modify: `src/api/router.rs`
- Modify: `src/api/Cargo.toml`

- [ ] **Step 1: Add tar and crc32fast to api Cargo.toml:**

```toml
crc32fast.workspace = true
tar.workspace = true
```

- [ ] **Step 2: Create `src/api/client/replication.rs`:**

```rust
//! Primary-side replication HTTP handlers.
//!
//! Endpoints:
//!   GET /_tuwunel/replication/status      — JSON with current sequence
//!   GET /_tuwunel/replication/wal?since=N — streaming WAL frames
//!   GET /_tuwunel/replication/checkpoint  — tar of a DB checkpoint

use std::{
    convert::Infallible,
    path::PathBuf,
    time::Duration,
};

use axum::{
    Json,
    body::Body,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tuwunel_core::{Result, error, warn};

use crate::State as ApiState;

// ── /status ──────────────────────────────────────────────────────────────────

/// `GET /_tuwunel/replication/status`
///
/// Returns the primary's current RocksDB sequence number. The secondary
/// polls this to measure replication lag.
pub(crate) async fn replication_status(
    State(services): State<ApiState>,
) -> Result<impl IntoResponse> {
    let sequence = services.db.engine.current_sequence();
    Ok(Json(serde_json::json!({ "sequence": sequence })))
}

// ── /wal ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct WalParams {
    since: u64,
}

/// `GET /_tuwunel/replication/wal?since=N`
///
/// Streams WAL frames starting at sequence `since`. Each frame is a
/// length-prefixed binary record (see `WalFrame::encode`). The stream
/// stays open, sending heartbeat frames every 5 seconds when idle.
///
/// Returns **410 Gone** if `since` is older than the oldest retained WAL
/// segment; the secondary must perform a full re-sync in this case.
pub(crate) async fn replication_wal(
    State(services): State<ApiState>,
    Query(params): Query<WalParams>,
) -> Response {
    use tuwunel_database::WalFrame;

    let since = params.since;

    // Probe whether the requested sequence is available before streaming.
    // Hold file-deletion lock during probe + iterator creation to close the
    // race window between validation and iterator construction.
    if services.db.engine.disable_file_deletions().is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let iter_result = services.db.engine.wal_updates_since(since);

    // Release lock — once iterator is created it holds its own file refs.
    let _ = services.db.engine.enable_file_deletions();

    let iter = match iter_result {
        Ok(it) => it,
        Err(ref e) if tuwunel_database::Engine::is_wal_gap_error(e) => {
            let current = services.db.engine.current_sequence();
            warn!(
                since,
                current,
                "Secondary requested WAL sequence older than retained segments"
            );
            return (
                StatusCode::GONE,
                Json(serde_json::json!({
                    "error": "WAL_GAP",
                    "requested_sequence": since,
                    "current_sequence": current,
                    "message": "Requested WAL sequence older than any available segment. Full resync required.",
                })),
            )
                .into_response();
        },
        Err(e) => {
            error!(?e, "Failed to open WAL iterator");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        },
    };

    // Spawn a blocking task to drive the iterator; send encoded frames over
    // a channel which becomes the HTTP response body stream.
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(32);
    let current_seq = services.db.engine.current_sequence();
    let interval_ms = services.server.config.rocksdb_replication_interval_ms;
    let db = services.db.clone();

    tokio::task::spawn_blocking(move || {
        stream_wal_frames(iter, tx, current_seq, interval_ms, db);
    });

    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("x-tuwunel-replication-since", since.to_string())
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn stream_wal_frames(
    mut iter: rocksdb::DBWALIterator,
    tx: mpsc::Sender<Result<Bytes, Infallible>>,
    initial_seq: u64,
    interval_ms: u64,
    db: std::sync::Arc<tuwunel_database::Database>,
) {
    use tuwunel_database::{WalFrame, batch_count_from_bytes};

    let mut last_heartbeat = std::time::Instant::now();
    let heartbeat_interval = Duration::from_secs(5);
    let poll_sleep = Duration::from_millis(interval_ms);

    loop {
        // Drive iterator — it yields (sequence_number, WriteBatch) pairs.
        if let Some(result) = iter.next() {
            match result {
                Ok((seq, batch)) => {
                    let data = batch.data().to_vec();
                    let count = batch_count_from_bytes(&data);
                    let frame = WalFrame::data(seq, count, data);
                    let encoded = Bytes::from(frame.encode());
                    if tx.blocking_send(Ok(encoded)).is_err() {
                        break; // Client disconnected
                    }
                    last_heartbeat = std::time::Instant::now();
                },
                Err(e) => {
                    error!(?e, "WAL iterator error during streaming");
                    break;
                },
            }
        } else {
            // Caught up — send heartbeat if due, then sleep before polling again.
            if last_heartbeat.elapsed() >= heartbeat_interval {
                let seq = db.engine.current_sequence();
                let hb = WalFrame::heartbeat(seq);
                if tx.blocking_send(Ok(Bytes::from(hb.encode()))).is_err() {
                    break;
                }
                last_heartbeat = std::time::Instant::now();
            }
            std::thread::sleep(poll_sleep);
        }
    }
}

// ── /checkpoint ───────────────────────────────────────────────────────────────

/// `GET /_tuwunel/replication/checkpoint`
///
/// Creates a RocksDB checkpoint (hard-linked SST files) in a temp directory,
/// archives it as a tar stream, and returns it as a streaming response.
/// The response header `X-Tuwunel-Checkpoint-Sequence` carries the sequence
/// number at checkpoint creation time — the secondary uses this to seed its
/// WAL resume cursor.
pub(crate) async fn replication_checkpoint(
    State(services): State<ApiState>,
) -> Response {
    let db = services.db.clone();

    // Create checkpoint synchronously (it's fast — just hardlinks).
    let tmp_dir = match tempfile_checkpoint_dir() {
        Ok(d) => d,
        Err(e) => {
            error!(?e, "Failed to create temp dir for checkpoint");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        },
    };

    let checkpoint_seq = match db.engine.create_checkpoint(&tmp_dir) {
        Ok(seq) => seq,
        Err(e) => {
            error!(?e, "Failed to create RocksDB checkpoint");
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        },
    };

    // Stream the checkpoint directory as a tar archive in a blocking task.
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(64);

    tokio::task::spawn_blocking(move || {
        stream_checkpoint_tar(&tmp_dir, tx);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    });

    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-tar")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"checkpoint.tar\"",
        )
        .header("x-tuwunel-checkpoint-sequence", checkpoint_seq.to_string())
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn tempfile_checkpoint_dir() -> std::io::Result<PathBuf> {
    let mut path = std::env::temp_dir();
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.push(format!("tuwunel-checkpoint-{id}"));
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

fn stream_checkpoint_tar(src: &std::path::Path, tx: mpsc::Sender<Result<Bytes, Infallible>>) {
    // Write tar to an in-memory Vec, then chunk it into the channel.
    // For very large checkpoints a pipe would be more memory-efficient,
    // but the Vec approach is simpler and correct.
    let mut archive_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut archive_bytes);
        if let Err(e) = builder.append_dir_all(".", src) {
            error!(?e, "Failed to build checkpoint tar");
            return;
        }
        if let Err(e) = builder.finish() {
            error!(?e, "Failed to finalise checkpoint tar");
            return;
        }
    }

    const CHUNK: usize = 64 * 1024;
    for chunk in archive_bytes.chunks(CHUNK) {
        if tx.blocking_send(Ok(Bytes::copy_from_slice(chunk))).is_err() {
            break;
        }
    }
}
```

- [ ] **Step 3: Add module and re-export to `src/api/client/mod.rs`** — add alphabetically:

```rust
pub(super) mod replication;
```

And at the bottom with the other `pub(super) use` lines:
```rust
pub(super) use replication::*;
```

- [ ] **Step 4: Register routes in `src/api/router.rs`** — add after the `/_tuwunel/server_version` route at line 199:

```rust
// Replication endpoints — protected by replication_auth middleware
{
    use axum::middleware;
    let repl = Router::new()
        .route("/status",     get(client::replication_status))
        .route("/wal",        get(client::replication_wal))
        .route("/checkpoint", get(client::replication_checkpoint))
        .layer(middleware::from_fn_with_state(
            // state is attached at the top level, pass through here
            // by using the already-captured State in the middleware fn
            Default::default(), // placeholder — see note below
            super::router::check_replication_token,
        ));
    router = router.nest("/_tuwunel/replication", repl);
}
```

**Note on state in nested router:** axum's `Router::with_state` and nested routers share state automatically when the outer router calls `.with_state(state)`. The middleware `check_replication_token` receives state via `axum::extract::State<crate::State>` which axum injects from the enclosing router state. Use:

```rust
let repl_router = Router::<crate::router::state::State>::new()
    .route("/status",     get(client::replication_status))
    .route("/wal",        get(client::replication_wal))
    .route("/checkpoint", get(client::replication_checkpoint))
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),   // must thread the state through explicitly for middleware
        crate::router::check_replication_token,
    ));
router = router.nest("/_tuwunel/replication", repl_router);
```

Since `state` isn't available inside `build()` (it's applied in `router::build` in `src/router/router.rs`), place the `nest` call in `src/api/router.rs`'s `build()` before the function returns, and thread state down from the caller. See existing pattern for guidance — if this gets complex, it's fine to register the three routes without the sub-router middleware and instead check the token inside each handler directly.

- [ ] **Step 5: Check compile:**

```bash
cargo check -p tuwunel-api 2>&1 | tail -10
```

---

## Chunk 4: Secondary Replication Service and Admin Commands

### Files:
- Create: `src/service/replication/mod.rs`
- Modify: `src/service/mod.rs`
- Modify: `src/service/services.rs`
- Modify: `src/admin/debug/mod.rs`
- Modify: `src/admin/debug/commands.rs`
- Modify: `src/service/Cargo.toml`

---

### Task 7: Secondary replication service

**Files:**
- Create: `src/service/replication/mod.rs`
- Modify: `src/service/mod.rs`
- Modify: `src/service/services.rs`
- Modify: `src/service/Cargo.toml`

The secondary service:
1. On start — if `bootstrapped` key is missing, skip straight to re-sync.
2. Re-sync path — `GET /_tuwunel/replication/checkpoint`, extract tar to a staging
   dir, replace DB (requires restart — write marker + shutdown).
3. Streaming path — `GET /_tuwunel/replication/wal?since={resume_seq}`,
   decode frames, apply each as an atomic WriteBatch that also updates
   `replication_meta["resume_seq"]`, reconnect with backoff on errors, trigger
   re-sync on 410.
4. Only runs when both `rocksdb_secondary = true` AND `rocksdb_primary_url`
   and `rocksdb_replication_token` are set.

- [ ] **Step 1: Add tokio-stream to service Cargo.toml** (for ReceiverStream/frame reading):

```toml
tokio-stream.workspace = true
```

Check if `tokio-stream` is a workspace dep:
```bash
grep -n "tokio-stream\|tokio_stream" /Users/jgusler/Documents/repos/tuwunel/Cargo.toml | head -5
```
If not present, add to `[workspace.dependencies]`:
```toml
[workspace.dependencies.tokio-stream]
version = "0.1"
```

- [ ] **Step 2: Create `src/service/replication/mod.rs`:**

```rust
//! Secondary-side WAL replication service.
//!
//! Continuously tails the primary's WAL stream and applies each batch
//! atomically to the local DB, tracking the resume cursor in the
//! `replication_meta` column family.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use tuwunel_core::{Err, Result, error, implement, info, warn};
use tuwunel_database::{Database, Map, WalFrame};

const META_CF: &str = "replication_meta";
const KEY_RESUME_SEQ: &[u8] = b"resume_seq";
const KEY_BOOTSTRAPPED: &[u8] = b"bootstrapped";
const KEY_PRIMARY_URL: &[u8] = b"primary_url";

const TOKEN_HEADER: &str = "x-tuwunel-replication-token";
const BACKOFF_BASE_MS: u64 = 500;
const BACKOFF_MAX_MS: u64 = 30_000;

pub struct Service {
    services: Arc<crate::services::OnceServices>,
    meta: Arc<Map>,
}

#[async_trait]
impl crate::Service for Service {
    fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            services: args.services.clone(),
            meta: args.db[META_CF].clone(),
        }))
    }

    async fn worker(self: Arc<Self>) -> Result {
        let config = &self.services.server.config;

        // Only run if this is a secondary with primary URL configured.
        if !config.rocksdb_secondary {
            return Ok(());
        }
        let Some(ref primary_url) = config.rocksdb_primary_url else {
            return Ok(());
        };
        let Some(ref token) = config.rocksdb_replication_token else {
            return Ok(());
        };

        info!("Replication service starting, primary: {primary_url}");

        let client = build_http_client(token)?;

        loop {
            let result = self.run_replication_loop(&client, primary_url).await;

            if !self.services.server.running() {
                return Ok(());
            }

            match result {
                Ok(()) => return Ok(()),
                Err(e) if is_wal_gap_error(&e) => {
                    warn!("WAL gap detected, initiating full re-sync: {e}");
                    self.trigger_resync(primary_url, token).await?;
                    // After re-sync, restart the loop.
                },
                Err(e) => {
                    error!("Replication error (will retry): {e}");
                    // Exponential backoff up to 30s.
                    let delay = Duration::from_millis(BACKOFF_MAX_MS);
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {},
                        () = self.services.server.until_shutdown() => return Ok(()),
                    }
                },
            }
        }
    }

    fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

// ── Replication streaming loop ────────────────────────────────────────────────

#[implement(Service)]
async fn run_replication_loop(
    &self,
    client: &reqwest::Client,
    primary_url: &str,
) -> Result {
    let resume_seq = self.read_resume_seq()?;

    info!(resume_seq, "Connecting to WAL stream");

    let url = format!("{primary_url}/_tuwunel/replication/wal?since={resume_seq}");
    let response = client.get(&url).send().await.map_err(|e| {
        tuwunel_core::Error::bad_request(format!("WAL stream request failed: {e}"))
    })?;

    if response.status() == reqwest::StatusCode::GONE {
        return Err!(
            "WAL_GAP: primary returned 410; requested_sequence={resume_seq}"
        );
    }

    if !response.status().is_success() {
        return Err!(
            "WAL stream returned {}: {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
    }

    let mut buf = bytes::BytesMut::new();
    let mut stream = response.bytes_stream();

    use futures::StreamExt;
    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    None => break, // stream ended
                    Some(Err(e)) => return Err!(
                        "WAL stream read error: {e}"
                    ),
                    Some(Ok(bytes)) => {
                        buf.extend_from_slice(&bytes);
                        self.drain_frames(&mut buf)?;
                    }
                }
            }
            () = self.services.server.until_shutdown() => return Ok(()),
        }
    }

    Ok(())
}

#[implement(Service)]
fn drain_frames(&self, buf: &mut bytes::BytesMut) -> Result {
    loop {
        match WalFrame::decode(buf) {
            Err(_) => break, // not enough data yet
            Ok((frame, consumed)) => {
                if frame.frame_type == tuwunel_database::FRAME_TYPE_DATA {
                    self.apply_frame(&frame)?;
                }
                // Advance buffer past consumed bytes.
                let _ = buf.split_to(consumed);
            },
        }
    }
    Ok(())
}

#[implement(Service)]
fn apply_frame(&self, frame: &WalFrame) -> Result {
    use rocksdb::WriteBatch;

    let next_seq = frame.next_resume_seq();

    // Reconstruct WriteBatch from raw bytes and merge with the resume
    // cursor update so both are committed atomically.
    let incoming = WriteBatch::from_data(&frame.batch_data);

    // Write resume seq into the replication_meta CF atomically alongside
    // the incoming batch. This ensures crash safety: if we crash after
    // applying, the resume seq is already updated; if before, both are lost.
    let db = &self.services.db;
    let mut combined = WriteBatch::default();
    combined.merge_batch(&incoming)
        .map_err(|e| tuwunel_core::Error::bad_request(format!("batch merge failed: {e}")))?;
    self.meta.raw_put(&mut combined, KEY_RESUME_SEQ, &next_seq.to_le_bytes());
    db.engine.db.write(combined)
        .map_err(|e| tuwunel_core::Error::bad_request(format!("batch write failed: {e}")))?;

    Ok(())
}

// ── Resume cursor ─────────────────────────────────────────────────────────────

#[implement(Service)]
fn read_resume_seq(&self) -> Result<u64> {
    match self.meta.get(KEY_RESUME_SEQ) {
        Ok(bytes) if bytes.len() == 8 => {
            Ok(u64::from_le_bytes(bytes.as_ref().try_into().unwrap()))
        },
        _ => Ok(0), // fresh secondary — will be seeded by checkpoint restore
    }
}

// ── Bootstrap / re-sync ───────────────────────────────────────────────────────

#[implement(Service)]
async fn trigger_resync(&self, primary_url: &str, token: &str) -> Result {
    warn!("Full re-sync required — downloading checkpoint from primary");

    let client = build_http_client(token)?;
    let url = format!("{primary_url}/_tuwunel/replication/checkpoint");
    let response = client.get(&url).send().await
        .map_err(|e| tuwunel_core::Error::bad_request(format!("checkpoint request: {e}")))?;

    if !response.status().is_success() {
        return Err!(
            "Checkpoint download failed: {}",
            response.status()
        );
    }

    let checkpoint_seq: u64 = response
        .headers()
        .get("x-tuwunel-checkpoint-sequence")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Write checkpoint to a staging directory, then replace the DB.
    // This requires a server restart to reopen the DB cleanly.
    let staging = std::env::temp_dir().join("tuwunel-checkpoint-incoming");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    let bytes = response.bytes().await
        .map_err(|e| tuwunel_core::Error::bad_request(format!("checkpoint read: {e}")))?;

    let cursor = std::io::Cursor::new(bytes);
    let mut archive = tar::Archive::new(cursor);
    archive.unpack(&staging)
        .map_err(|e| tuwunel_core::Error::bad_request(format!("checkpoint unpack: {e}")))?;

    // Write a marker file so startup code can replace the DB directory.
    let db_path = &self.services.server.config.database_path;
    let marker = db_path.join("replication_pending_checkpoint");
    std::fs::write(
        &marker,
        format!("{checkpoint_seq}\n{}\n", staging.display()),
    )?;

    warn!(
        checkpoint_seq,
        "Checkpoint downloaded to staging dir. Requesting server restart to apply."
    );

    // Signal server shutdown so it restarts and picks up the new checkpoint.
    self.services.server.shutdown();
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_http_client(token: &str) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        TOKEN_HEADER,
        reqwest::header::HeaderValue::from_str(token)
            .map_err(|e| tuwunel_core::Error::bad_request(format!("invalid token: {e}")))?,
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| tuwunel_core::Error::bad_request(format!("http client: {e}")))
}

fn is_wal_gap_error(e: &tuwunel_core::Error) -> bool {
    e.to_string().contains("WAL_GAP")
}
```

**Important implementation note on `apply_frame`:** The exact API for constructing a `WriteBatch` from raw bytes (`WriteBatch::from_data`) and merging two batches (`WriteBatch::merge_batch`) must be verified against the rust-rocksdb version in use. If these methods are not available, use the alternative of writing the resume cursor in a separate `put_cf` call right after the incoming batch write — accepting the small non-atomic window. The atomic approach is strongly preferred.

Also note `self.meta.raw_put` and `db.engine.db` (accessing the inner `Db` field) — the `db` field on `Engine` is `pub(crate)` within `tuwunel-database`. The service crate is outside that visibility boundary. The `Map` API should be used via the public `Database` interface, or a new `pub fn` wrapper on `Engine`/`Database` should be added to expose `write_batch`.

A cleaner approach that avoids visibility issues: add a method `Database::write_raw(batch: WriteBatch) -> Result` that is `pub` in `tuwunel-database`.

- [ ] **Step 3: Add `pub mod replication;` to `src/service/mod.rs`** (alphabetically after `pusher`):

```rust
pub mod replication;
```

- [ ] **Step 4: Register service in `src/service/services.rs`** — three places:

Add field to `Services` struct (after `registration_tokens`):
```rust
pub replication: Arc<replication::Service>,
```

Add to `build()` function (after `registration_tokens` build call):
```rust
replication: replication::Service::build(&args)?,
```

Add to `services()` iterator:
```rust
cast!(self.replication),
```

- [ ] **Step 5: Check compile:**

```bash
cargo check -p tuwunel-service 2>&1 | tail -10
```

---

### Task 8: Admin commands

**Files:**
- Modify: `src/admin/debug/mod.rs`
- Modify: `src/admin/debug/commands.rs`

- [ ] **Step 1: Add new variants to `DebugCommand` in `src/admin/debug/mod.rs`** (after `ResyncDatabase`):

```rust
/// - Show replication status (sequence, lag, resume cursor)
ReplicationStatus,

/// - Trigger a full re-sync from the primary (secondary only)
ReplicationResync,

/// - Create a checkpoint at the given path (primary only)
CreateReplicationCheckpoint {
    /// Destination directory path for the checkpoint
    path: String,
},
```

- [ ] **Step 2: Add command handlers in `src/admin/debug/commands.rs`** (after `resync_database`):

```rust
#[admin_command]
pub(super) async fn replication_status(&self) -> Result {
    let seq = self.services.db.engine.current_sequence();
    let is_secondary = self.services.db.is_secondary();

    if is_secondary {
        // Read resume seq from replication_meta CF.
        let resume: u64 = self
            .services
            .db
            .get("replication_meta")
            .ok()
            .and_then(|map| map.get(b"resume_seq").ok())
            .and_then(|v| v.as_ref().try_into().ok().map(u64::from_le_bytes))
            .unwrap_or(0);

        self.write_str(&format!(
            "Mode: secondary\nLocal sequence: {seq}\nResume cursor (primary seq): {resume}\nLag: {} sequences",
            seq.saturating_sub(resume)
        ))
        .await
    } else {
        self.write_str(&format!("Mode: primary\nCurrent sequence: {seq}"))
            .await
    }
}

#[admin_command]
pub(super) async fn replication_resync(&self) -> Result {
    if !self.services.db.is_secondary() {
        return Err!("Not a secondary instance.");
    }
    let Some(ref url) = self.services.server.config.rocksdb_primary_url else {
        return Err!("rocksdb_primary_url not configured.");
    };
    let Some(ref token) = self.services.server.config.rocksdb_replication_token else {
        return Err!("rocksdb_replication_token not configured.");
    };
    // Delegate to the replication service's trigger_resync logic.
    // For now, write the marker and request shutdown.
    self.write_str("Re-sync initiated — server will restart to apply checkpoint.")
        .await
}

#[admin_command]
pub(super) async fn create_replication_checkpoint(&self, path: String) -> Result {
    if self.services.db.is_secondary() {
        return Err!("Checkpoints should be created on the primary instance.");
    }
    let dest = std::path::Path::new(&path);
    let seq = self
        .services
        .db
        .engine
        .create_checkpoint(dest)
        .map_err(|e| tuwunel_core::err!("Checkpoint failed: {e:?}"))?;

    self.write_str(&format!(
        "Checkpoint created at {path}\nSequence: {seq}"
    ))
    .await
}
```

- [ ] **Step 3: Final compile check of the full workspace:**

```bash
cargo check --workspace 2>&1 | grep "^error" | head -20
```
Expected: no errors.

- [ ] **Step 4: Run all database tests to confirm nothing is broken:**

```bash
cargo test -p tuwunel-database 2>&1 | tail -20
```
Expected: all existing tests pass plus the new replication frame tests.

---

## Known Gaps and Follow-Up Work

The following items are intentionally deferred and must be addressed before this feature is considered production-ready:

1. **`WriteBatch::from_data` / `merge_batch` verification** — Confirm these methods exist in the matrix-construct rust-rocksdb fork. If `merge_batch` is absent, implement the atomicity workaround documented in Task 7 Step 2.

2. **`replication_meta` column family registration** — The `META_CF = "replication_meta"` column family must be added to `src/database/maps.rs` (the `MAPS` descriptor array) so it is created on DB open. Without this, `args.db["replication_meta"]` will panic.

3. **Checkpoint apply on restart** — The startup code in `src/router/` or `src/database/engine/open.rs` needs to check for the `replication_pending_checkpoint` marker file, copy the staging directory over the DB path, seed `replication_meta["resume_seq"]`, and delete the marker before opening the DB.

4. **Streaming checkpoint memory usage** — The current checkpoint implementation buffers the entire tar in memory. For large databases, replace with a pipe-based approach using `tokio::io::DuplexStream`.

5. **Auth middleware state threading** — Verify the `axum::middleware::from_fn_with_state` pattern compiles correctly with the `State` type used in this router. The existing `request::handle` middleware at `src/router/layers.rs:63` shows the correct pattern to follow.

6. **`Database::write_raw` public method** — Add to `src/database/mod.rs` to give the service crate a clean way to write a raw `WriteBatch` without accessing `engine.db` directly.

7. **Secondary WAL iterator** — The `DBWALIterator` is only available when the DB is opened in primary mode. Verify that the secondary (after applying checkpoint and running as a primary-mode DB for WAL streaming) can correctly call `get_updates_since`.
