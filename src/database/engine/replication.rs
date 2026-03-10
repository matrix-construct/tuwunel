//! Engine-level replication primitives.
//!
//! Provides the WAL wire frame format, RocksDB checkpoint creation,
//! file-deletion guards, and WAL iterator access for the replication system.

use std::{
	path::Path,
	time::{SystemTime, UNIX_EPOCH},
};

use rocksdb::Checkpoint;
use tuwunel_core::{Err, Result, implement};

use super::Engine;
use crate::util::map_err;

// ── Wire frame ─────────────────────────────────────────────────────────────────

pub const FRAME_TYPE_DATA: u8 = 0x01;
pub const FRAME_TYPE_HEARTBEAT: u8 = 0x02;

/// Length of a frame header in bytes (everything before `batch_data`).
pub const FRAME_HEADER_LEN: usize = 33;

/// A single replication frame transmitted over the HTTP WAL stream.
///
/// Wire format (all integers little-endian):
/// ```text
/// offset  size  field
/// 0       1     frame_type: 0x01 = data, 0x02 = heartbeat
/// 1       8     sequence: primary's BatchResult sequence_number
/// 9       8     count: number of WAL sequence numbers consumed by this batch
/// 17      8     timestamp_ms: unix millis when primary wrote this
/// 25      4     crc32: crc32fast checksum of batch_data (0 for heartbeats)
/// 29      4     batch_len: byte length of batch_data (0 for heartbeats)
/// 33      ?     batch_data: raw WriteBatch serialization
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFrame {
	pub frame_type: u8,
	/// Primary's sequence number for the first record in this batch.
	pub sequence: u64,
	/// How many WAL sequence numbers this batch consumes.
	/// Secondary's next resume point = `sequence + count`.
	pub count: u64,
	/// Unix milliseconds when the primary wrote this batch.
	pub timestamp_ms: u64,
	/// CRC32 of `batch_data`. Zero for heartbeats.
	pub crc32: u32,
	/// Raw WriteBatch bytes. Empty for heartbeats.
	pub batch_data: Vec<u8>,
}

impl WalFrame {
	/// Create a heartbeat frame carrying the primary's current sequence.
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

	/// Create a data frame from a WAL batch. CRC is computed automatically.
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

	/// Returns the sequence number the secondary should use as its next
	/// `?since=` argument after successfully applying this frame.
	/// For heartbeats, returns `sequence` unchanged (cursor must not advance
	/// based on heartbeats alone).
	#[inline]
	pub fn next_resume_seq(&self) -> u64 {
		if self.frame_type == FRAME_TYPE_DATA {
			self.sequence.saturating_add(self.count)
		} else {
			self.sequence
		}
	}

	/// Encode the frame to bytes for writing to the HTTP stream.
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

	/// Attempt to decode a frame from the start of `buf`.
	///
	/// Returns `(frame, bytes_consumed)` on success. Returns `Err` if the
	/// buffer is too short to contain a complete frame, or if the CRC does
	/// not match.
	pub fn decode(buf: &[u8]) -> Result<(Self, usize)> {
		if buf.len() < FRAME_HEADER_LEN {
			return Err!(
				"WAL frame header truncated: {} bytes < {FRAME_HEADER_LEN} required",
				buf.len()
			);
		}

		let frame_type  = buf[0];
		let sequence    = u64::from_le_bytes(buf[1..9].try_into().expect("8 bytes"));
		let count       = u64::from_le_bytes(buf[9..17].try_into().expect("8 bytes"));
		let timestamp_ms = u64::from_le_bytes(buf[17..25].try_into().expect("8 bytes"));
		let crc32       = u32::from_le_bytes(buf[25..29].try_into().expect("4 bytes"));
		let batch_len   = u32::from_le_bytes(buf[29..33].try_into().expect("4 bytes")) as usize;

		let total = FRAME_HEADER_LEN + batch_len;
		if buf.len() < total {
			return Err!(
				"WAL frame body truncated: need {total} bytes, have {}",
				buf.len()
			);
		}

		let batch_data = buf[FRAME_HEADER_LEN..total].to_vec();

		if frame_type == FRAME_TYPE_DATA && !batch_data.is_empty() {
			let actual = crc32fast::hash(&batch_data);
			if actual != crc32 {
				return Err!(
					"WAL frame CRC mismatch: stored {crc32:#010x}, computed {actual:#010x}"
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
///
/// RocksDB `WriteBatch` layout: `[8 bytes sequence][4 bytes count][records…]`.
/// The count at bytes 8–11 is the number of operations in the batch, which
/// equals how many sequence numbers the batch consumes. Returns 0 if the
/// slice is too short to contain the count.
#[inline]
pub fn batch_count_from_bytes(data: &[u8]) -> u64 {
	if data.len() < 12 {
		return 0;
	}
	u32::from_le_bytes(data[8..12].try_into().expect("4 bytes")) as u64
}

fn now_ms() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_millis() as u64
}

// ── Engine methods ─────────────────────────────────────────────────────────────

/// Create a RocksDB checkpoint at `dest`.
///
/// A checkpoint is a consistent point-in-time snapshot consisting of
/// hard-links to existing SST files. It is created atomically and returns
/// the sequence number at checkpoint creation time.
#[implement(Engine)]
pub fn create_checkpoint(&self, dest: &Path) -> Result<u64> {
	let checkpoint = Checkpoint::new(&self.db).map_err(map_err)?;
	checkpoint.create_checkpoint(dest).map_err(map_err)?;
	Ok(self.db.latest_sequence_number())
}

/// Prevent RocksDB from deleting obsolete files.
///
/// Call this before initiating a checkpoint transfer or live-file listing
/// to ensure files are not removed while they are being transferred. Must
/// be paired with a subsequent `enable_file_deletions` call.
#[implement(Engine)]
pub fn disable_file_deletions(&self) -> Result {
	self.db.disable_file_deletions().map_err(map_err)
}

/// Re-enable file deletion after a `disable_file_deletions` call.
#[implement(Engine)]
pub fn enable_file_deletions(&self) -> Result {
	self.db.enable_file_deletions().map_err(map_err)
}

/// Returns the current latest WAL sequence number of this instance.
///
/// Used by the primary to populate heartbeat frames so the secondary knows
/// the primary is alive and can gauge replication lag.
#[implement(Engine)]
pub fn latest_wal_sequence(&self) -> u64 { self.db.latest_sequence_number() }

/// Return a WAL iterator starting at `since`.
///
/// Yields batches whose sequence number is >= `since`. If `since` is older
/// than the oldest retained WAL segment, this returns `Err` — call
/// `is_wal_gap_error` on the result to distinguish this case from other
/// errors.
#[implement(Engine)]
pub fn wal_updates_since(&self, since: u64) -> Result<rocksdb::DBWALIterator> {
	self.db.get_updates_since(since).map_err(map_err)
}

/// Return a higher-level iterator of [`WalFrame`]s starting at `since`.
///
/// Wraps `wal_updates_since` and maps each rocksdb batch into a `WalFrame`,
/// hiding the internal `DBWALIterator` / `WriteBatch` types from callers
/// that only have access to `tuwunel-database` (not `rust-rocksdb` directly).
///
/// Returns `Err` immediately if the sequence is too old; call
/// `is_wal_gap_error` to distinguish a gap from other errors.
#[implement(Engine)]
pub fn wal_frame_iter(
	&self,
	since: u64,
) -> Result<Box<dyn Iterator<Item = Result<WalFrame>> + Send>> {
	let iter = self.db.get_updates_since(since).map_err(map_err)?;
	Ok(Box::new(iter.map(|result| {
		result
			.map(|(seq, batch)| {
				let data = batch.data().to_vec();
				let count = batch_count_from_bytes(&data);
				WalFrame::data(seq, count, data)
			})
			.map_err(map_err)
	})))
}

/// Returns `true` if `err` indicates the requested WAL sequence is older
/// than any retained segment on this instance.
///
/// The primary uses this to return HTTP 410 rather than 500.
pub fn is_wal_gap_error(err: &tuwunel_core::Error) -> bool {
	let msg = err.to_string().to_lowercase();
	msg.contains("too old")
		|| msg.contains("older than")
		|| msg.contains("sequence not")
		|| msg.contains("data loss")
		|| msg.contains("not available")
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn heartbeat_round_trip() {
		let frame = WalFrame::heartbeat(12345);
		let encoded = frame.encode();
		let (decoded, consumed) = WalFrame::decode(&encoded).unwrap();
		assert_eq!(consumed, encoded.len());
		assert_eq!(decoded.frame_type, FRAME_TYPE_HEARTBEAT);
		assert_eq!(decoded.sequence, 12345);
		assert_eq!(decoded.count, 0);
		assert!(decoded.batch_data.is_empty());
		// Heartbeat does not advance resume cursor
		assert_eq!(decoded.next_resume_seq(), 12345);
	}

	#[test]
	fn data_frame_round_trip() {
		let data = b"test writebatch payload bytes".to_vec();
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
	fn data_frame_zero_count() {
		// A batch with count=0 should not advance the cursor beyond sequence.
		let frame = WalFrame::data(500, 0, b"payload".to_vec());
		assert_eq!(frame.next_resume_seq(), 500);
	}

	#[test]
	fn crc_mismatch_rejected() {
		let frame = WalFrame::data(1000, 1, b"payload data".to_vec());
		let mut encoded = frame.encode();
		// Corrupt one byte in the batch_data region (after the 33-byte header).
		let last = encoded.len() - 1;
		encoded[last] ^= 0xFF;
		assert!(WalFrame::decode(&encoded).is_err());
	}

	#[test]
	fn truncated_header_rejected() {
		let frame = WalFrame::heartbeat(1);
		let encoded = frame.encode();
		assert!(WalFrame::decode(&encoded[..FRAME_HEADER_LEN - 1]).is_err());
	}

	#[test]
	fn truncated_body_rejected() {
		let frame = WalFrame::data(1, 1, b"hello world test".to_vec());
		let mut encoded = frame.encode();
		encoded.truncate(encoded.len() - 3);
		assert!(WalFrame::decode(&encoded).is_err());
	}

	#[test]
	fn multiple_frames_in_buffer() {
		let f1 = WalFrame::data(100, 5, b"batch one".to_vec());
		let f2 = WalFrame::heartbeat(105);
		let f3 = WalFrame::data(105, 3, b"batch two".to_vec());
		let mut buf = f1.encode();
		buf.extend_from_slice(&f2.encode());
		buf.extend_from_slice(&f3.encode());

		let (d1, c1) = WalFrame::decode(&buf).unwrap();
		let (d2, c2) = WalFrame::decode(&buf[c1..]).unwrap();
		let (d3, c3) = WalFrame::decode(&buf[c1 + c2..]).unwrap();

		assert_eq!(d1.sequence, 100);
		assert_eq!(d2.frame_type, FRAME_TYPE_HEARTBEAT);
		assert_eq!(d3.sequence, 105);
		assert_eq!(c1 + c2 + c3, buf.len());
	}

	#[test]
	fn batch_count_from_bytes_valid() {
		let mut fake = vec![0u8; 16];
		fake[8..12].copy_from_slice(&7u32.to_le_bytes());
		assert_eq!(batch_count_from_bytes(&fake), 7);
	}

	#[test]
	fn batch_count_from_bytes_too_short() {
		assert_eq!(batch_count_from_bytes(&[0u8; 5]), 0);
		assert_eq!(batch_count_from_bytes(&[]), 0);
	}
}
