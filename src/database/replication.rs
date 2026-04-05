//! Engine-level replication primitives.
//!
//! Provides the WAL wire frame format, RocksDB checkpoint creation,
//! file-deletion guards, and WAL iterator access for the replication system.

#[cfg(test)]
mod tests;

use minicbor::{
	Decode, Encode,
	decode::Decoder,
	encode::{Encoder, Write},
};
use tuwunel_core::{Result, err, implement, utils::time::now_millis};

use crate::{
	engine::{Engine, wal::batch_count_from_bytes},
	util::map_err,
};

/// A single replication frame transmitted over the HTTP WAL stream.
#[derive(Clone, Debug, Decode, Default, Encode, Eq, PartialEq)]
pub struct WalFrame {
	/// Primary's sequence number for the first record in this batch.
	#[n(0)]
	pub sequence: u64,

	/// How many WAL sequence numbers this batch consumes.
	/// Secondary's next resume point = `sequence + count`.
	#[n(1)]
	pub count: u64,

	/// Unix milliseconds when the primary wrote this batch.
	#[n(2)]
	pub timestamp_ms: u64,

	/// Raw WriteBatch bytes. Empty for heartbeats.
	#[n(3)]
	pub batch_data: Vec<u8>,
}

/// Type of frame. Derived from the payload of a frame; not physically
/// represented in the actual frame.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum FrameKind {
	/// Frame contains no data and does not advance the sequence.
	#[default]
	HeartBeat,

	/// Frame contains data and a positive count advancing the sequence.
	Data,
}

impl WalFrame {
	/// Create a heartbeat frame carrying the primary's current sequence.
	#[inline]
	#[must_use]
	pub fn heartbeat(primary_sequence: u64) -> Self {
		Self {
			sequence: primary_sequence,
			timestamp_ms: now_millis(),
			..Self::default()
		}
	}

	/// Create a data frame from a WAL batch.
	#[inline]
	#[must_use]
	pub fn data(sequence: u64, count: u64, batch_data: Vec<u8>) -> Self {
		Self {
			sequence,
			count,
			timestamp_ms: now_millis(),
			batch_data,
		}
	}

	/// Attempt to decode a frame from the start of `buf`.
	///
	/// Returns `(frame, bytes_consumed)` on success. Returns `Err` if the
	/// buffer is too short to contain a complete frame.
	#[tracing::instrument(level = "trace", skip_all, ret(Debug))]
	pub fn decode(buf: &[u8]) -> Result<(Self, &[u8])> {
		let mut decoder = Decoder::new(buf);
		let value: Self = decoder
			.decode()
			.map_err(|e| err!("Failed to decode WalFrame: {e}"))?;

		Ok((value, &buf[decoder.position()..]))
	}

	/// Encode the frame to bytes for writing to the HTTP stream.
	pub fn encode<W: Write>(&self, out: W) -> Result<W> {
		let mut encoder = Encoder::new(out);

		encoder
			.encode(self)
			.map_err(|_| err!("Failed to encode WalFrame"))?;

		Ok(encoder.into_writer())
	}

	/// Encode the frame to a vector of bytes for writing to the HTTP stream.
	#[inline]
	pub fn encode_to_vec(&self) -> Result<Vec<u8>> {
		let mut vec = Vec::new();
		self.encode(&mut vec)?;

		Ok(vec)
	}

	/// Determine the type of the frame based on its contents.
	#[inline]
	#[must_use]
	pub fn kind(&self) -> FrameKind {
		if self.count > 0 || !self.batch_data.is_empty() {
			assert!(
				self.batch_data.is_empty() || self.count > 0,
				"expected sequence advance when receiving data",
			);

			FrameKind::Data
		} else {
			FrameKind::HeartBeat
		}
	}

	/// Returns the sequence number the secondary should use as its next
	/// `?since=` argument after successfully applying this frame.
	/// For heartbeats, returns `sequence` unchanged (cursor must not advance
	/// based on heartbeats alone).
	#[inline]
	#[must_use]
	#[tracing::instrument(
		level = "trace",
		ret(level = "trace"),
		skip_all,
		fields(
			seq = self.sequence,
			count = self.count,
		)
	)]
	pub fn next_resume_seq(&self) -> u64 {
		if self.kind() == FrameKind::Data {
			self.sequence.saturating_add(self.count)
		} else {
			self.sequence
		}
	}
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
) -> Result<impl Iterator<Item = Result<WalFrame>> + Send> {
	let iter = self.wal_updates_since(since)?;

	Ok(SendWalIter(iter))
}

/// Newtype wrapper making `DBWALIterator` safe to send across threads.
///
/// `DBWALIterator` holds a `*mut rocksdb_wal_iterator_t` raw pointer which
/// is not auto-`Send`. RocksDB WAL iterators are not concurrently shared;
/// this iterator is consumed by exactly one thread at a time, so sending
/// ownership across a thread boundary is safe.
struct SendWalIter(rocksdb::DBWALIterator);

// SAFETY: DBWALIterator is not auto-Send due to its raw pointer, but the
// underlying RocksDB iterator is safe to use from whichever single thread
// owns it at any given time. We never share it across threads simultaneously.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for SendWalIter {}

impl Iterator for SendWalIter {
	type Item = Result<WalFrame>;

	fn next(&mut self) -> Option<Self::Item> {
		self.0.next().map(|result| {
			result.map_err(map_err).map(|(seq, batch)| {
				let data = batch.data().to_vec();
				let count = batch_count_from_bytes(&data);
				WalFrame::data(seq, count, data)
			})
		})
	}
}
