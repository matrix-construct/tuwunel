//! Applying the primary's WAL on a replica.
//!
//! A replica used to apply each frame from the primary as two writes: the
//! primary's batch, then its resume cursor. Here both go into one RocksDB
//! write, and the batches of several frames can be combined into that same
//! write ([`RawBatchGroup`]), so a whole network read costs one write.
//!
//! The primary's batches are combined byte for byte, the way RocksDB's own
//! `WriteBatchInternal::Append` does it: a `WriteBatch` is
//! `[8 bytes sequence][4 bytes LE count][records…]`, so the combination is one
//! header with the summed count followed by every batch's records, in order.
//! Nothing is decoded and re-encoded, so every record type the primary wrote
//! (puts, deletes, single deletes, range deletes, merges, log data, …) is
//! carried exactly as it was. The sequence field is left zero: RocksDB assigns
//! it on write.

use rocksdb::{AsColumnFamilyRef, WriteBatch, WriteOptions};
use tuwunel_core::{Result, err};

use crate::{Database, util::map_err};

/// Length of a `WriteBatch` header: 8 bytes sequence, 4 bytes count.
const HEADER_LEN: usize = 12;

/// Key of the replica's resume cursor in `replication_meta`.
pub(crate) const RESUME_SEQ_KEY: &[u8] = b"primary_resume_seq";

/// Most bytes of primary records combined into one write. A single batch
/// larger than this is still applied, on its own.
pub const GROUP_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Most primary batches combined into one write.
pub const GROUP_MAX_BATCHES: usize = 512;

/// Several of the primary's raw `WriteBatch`es combined into one, in order.
#[derive(Debug)]
pub struct RawBatchGroup {
	/// Header (count filled in when built) followed by every batch's records.
	rep: Vec<u8>,
	/// Summed operation count of the batches appended.
	count: u32,
	/// Number of batches appended.
	batches: usize,
	max_bytes: usize,
	max_batches: usize,
}

impl Default for RawBatchGroup {
	fn default() -> Self { Self::new() }
}

impl RawBatchGroup {
	/// An empty group bounded by [`GROUP_MAX_BYTES`] and [`GROUP_MAX_BATCHES`].
	#[must_use]
	pub fn new() -> Self { Self::with_limits(GROUP_MAX_BYTES, GROUP_MAX_BATCHES) }

	/// An empty group with the given bounds.
	#[must_use]
	pub fn with_limits(max_bytes: usize, max_batches: usize) -> Self {
		Self {
			rep: vec![0; HEADER_LEN],
			count: 0,
			batches: 0,
			max_bytes,
			max_batches,
		}
	}

	/// Append one raw `WriteBatch` from the primary.
	///
	/// Returns `Ok(false)`, appending nothing, when the batch would take the
	/// group past its bounds; it then belongs in the next group. The first
	/// batch of a group is always taken, whatever its size, so a group never
	/// refuses everything. Returns `Err` for bytes too short to be a
	/// `WriteBatch` at all.
	pub fn try_append(&mut self, batch: &[u8]) -> Result<bool> {
		if batch.len() < HEADER_LEN {
			return Err(err!(Database(
				"Malformed WriteBatch from primary: {} bytes, shorter than its \
				 {HEADER_LEN}-byte header",
				batch.len()
			)));
		}

		let count = u32::from_le_bytes(batch[8..12].try_into().expect("4 bytes"));
		let records = &batch[HEADER_LEN..];
		let Some(total) = self.count.checked_add(count) else {
			return Ok(false);
		};

		if self.batches > 0
			&& (self.batches >= self.max_batches
				|| self.records_len().saturating_add(records.len()) > self.max_bytes)
		{
			return Ok(false);
		}

		self.rep.extend_from_slice(records);
		self.count = total;
		self.batches = self.batches.saturating_add(1);
		Ok(true)
	}

	/// Whether no batch has been appended.
	#[must_use]
	pub fn is_empty(&self) -> bool { self.batches == 0 }

	/// Number of batches appended.
	#[must_use]
	pub fn batches(&self) -> usize { self.batches }

	/// Summed operation count, i.e. the sequences the group's batches consume.
	#[must_use]
	pub fn count(&self) -> u32 { self.count }

	/// Bytes of records appended (excluding the header).
	#[must_use]
	pub fn records_len(&self) -> usize { self.rep.len().saturating_sub(HEADER_LEN) }

	/// The combined `WriteBatch`, plus a put of the resume cursor into `cf`
	/// (`replication_meta`) after every record when `cursor` is given.
	pub(crate) fn into_write_batch(
		mut self,
		cf: &impl AsColumnFamilyRef,
		cursor: Option<u64>,
	) -> WriteBatch {
		self.rep[8..HEADER_LEN].copy_from_slice(&self.count.to_le_bytes());
		let mut batch = WriteBatch::from_data(&self.rep);
		if let Some(cursor) = cursor {
			batch.put_cf(cf, RESUME_SEQ_KEY, cursor.to_le_bytes());
		}
		batch
	}
}

impl Database {
	/// Apply one raw `WriteBatch` from the primary's WAL stream together with
	/// the resume cursor it brings this replica to, as a single atomic write.
	///
	/// An empty `data` writes only the cursor.
	pub fn write_raw_batch_with_cursor(&self, data: &[u8], cursor: u64) -> Result {
		let mut group = RawBatchGroup::new();
		if !data.is_empty() {
			// The first batch of a group is always taken.
			group.try_append(data)?;
		}
		self.write_raw_group(group, Some(cursor))
	}

	/// Apply a group of the primary's batches, and the resume cursor when
	/// `cursor` is given, as a single atomic write: the cursor can never be
	/// on disk without the data it describes, nor the data without it.
	///
	/// Bypasses `Map` on purpose: applied primary batches are not local writes
	/// (see `rejoin::Purity`), and the cursor is node-local.
	pub fn write_raw_group(&self, group: RawBatchGroup, cursor: Option<u64>) -> Result {
		if group.is_empty() && cursor.is_none() {
			return Ok(());
		}

		if cursor.is_some() {
			self.engine
				.purity
				.note_write("replication_meta", RESUME_SEQ_KEY);
		}

		let cf = self.engine.cf("replication_meta");
		let batch = group.into_write_batch(&cf, cursor);
		self.engine
			.db
			.write_opt(&batch, &WriteOptions::default())
			.map_err(map_err)?;

		// The database runs with `manual_wal_flush = true`, so the write above
		// only reached RocksDB's in-process WAL buffer. Push it to the WAL file
		// now (no fsync), as `Map::insert` does after each write, so applied
		// data and its cursor survive this process dying: a replica that
		// crashed with them still buffered would come back behind the cursor
		// its primary had already moved past for it. Done unconditionally,
		// even while corked, since nothing else on this path flushes.
		self.engine.flush()
	}
}

#[cfg(test)]
mod tests {
	use std::{collections::BTreeMap, fs, path::PathBuf};

	use rocksdb::{ColumnFamilyDescriptor, DB, IteratorMode, Options, WriteBatch, WriteOptions};
	use tuwunel_core::utils::rand;

	use super::*;

	const COLUMNS: &[&str] = &["global", "pduid_pdu", "replication_meta"];
	const DATA_COLUMNS: &[&str] = &["global", "pduid_pdu"];

	type Rows = Vec<(Vec<u8>, Vec<u8>)>;

	struct TestDb {
		dir: PathBuf,
		db: Option<DB>,
	}

	impl TestDb {
		fn new(tag: &str) -> Self {
			let dir = std::env::temp_dir().join(format!(
				"tuwunel-apply-test-{tag}-{}-{}",
				std::process::id(),
				rand::string(8)
			));
			fs::remove_dir_all(&dir).ok();
			let mut opts = Options::default();
			opts.create_if_missing(true);
			opts.create_missing_column_families(true);
			opts.set_wal_ttl_seconds(3600);
			opts.set_manual_wal_flush(true);
			let cfs = COLUMNS
				.iter()
				.map(|name| ColumnFamilyDescriptor::new(*name, Options::default()));
			let db = DB::open_cf_descriptors(&opts, dir.join("data"), cfs).unwrap();
			Self { dir, db: Some(db) }
		}

		fn db(&self) -> &DB { self.db.as_ref().unwrap() }

		fn write(&self, batch: &WriteBatch) {
			self.db()
				.write_opt(batch, &WriteOptions::default())
				.unwrap();
		}

		fn seq(&self) -> u64 { self.db().latest_sequence_number() }

		/// Every primary batch after `after`: `(sequence, count, raw bytes)`.
		fn wal(&self, after: u64) -> Vec<(u64, u64, Vec<u8>)> {
			// `manual_wal_flush`: the WAL iterator only sees what was flushed.
			self.db().flush_wal(false).unwrap();
			self.db()
				.get_updates_since(after)
				.unwrap()
				.map(|r| {
					let (seq, batch) = r.unwrap();
					let data = batch.data().to_vec();
					let count = u64::from(u32::from_le_bytes(data[8..12].try_into().unwrap()));
					(seq, count, data)
				})
				.collect()
		}

		/// Apply a group the way `Database::write_raw_group` does.
		fn apply(&self, group: RawBatchGroup, cursor: Option<u64>) {
			let cf = self.db().cf_handle("replication_meta").unwrap();
			self.write(&group.into_write_batch(&cf, cursor));
			self.db().flush_wal(false).unwrap();
		}

		fn cursor(&self) -> Option<u64> {
			let cf = self.db().cf_handle("replication_meta").unwrap();
			self.db()
				.get_cf(&cf, RESUME_SEQ_KEY)
				.unwrap()
				.map(|v| u64::from_le_bytes(v[..8].try_into().unwrap()))
		}

		/// Contents of the primary's (non node-local) columns.
		fn contents(&self) -> BTreeMap<String, Rows> {
			DATA_COLUMNS
				.iter()
				.map(|name| {
					let cf = self.db().cf_handle(name).unwrap();
					let rows = self
						.db()
						.iterator_cf(&cf, IteratorMode::Start)
						.map(|r| {
							let (k, v) = r.unwrap();
							(k.to_vec(), v.to_vec())
						})
						.collect();
					((*name).to_owned(), rows)
				})
				.collect()
		}
	}

	impl Drop for TestDb {
		fn drop(&mut self) {
			self.db = None;
			fs::remove_dir_all(&self.dir).ok();
		}
	}

	/// A primary with a varied history: single puts, a multi-column batch,
	/// overwrites, a delete, a range delete and a single delete.
	fn primary() -> TestDb {
		let p = TestDb::new("primary");
		let cf = |name| p.db().cf_handle(name).unwrap();

		p.db()
			.put_cf(&cf("pduid_pdu"), b"a", b"1")
			.unwrap();
		p.db().put_cf(&cf("global"), b"g", b"1").unwrap();

		let mut batch = WriteBatch::default();
		batch.put_cf(&cf("pduid_pdu"), b"b", b"2");
		batch.put_cf(&cf("pduid_pdu"), b"c", b"3");
		batch.put_cf(&cf("global"), b"h", b"2");
		batch.put_cf(&cf("pduid_pdu"), b"a", b"overwritten");
		p.write(&batch);

		p.db().delete_cf(&cf("pduid_pdu"), b"b").unwrap();

		for key in [b"r1", b"r2", b"r3", b"zz"] {
			p.db()
				.put_cf(&cf("pduid_pdu"), key, b"r")
				.unwrap();
		}
		let mut batch = WriteBatch::default();
		batch.delete_range_cf(&cf("pduid_pdu"), b"r1", b"r3");
		batch.put_cf(&cf("global"), b"i", b"3");
		p.write(&batch);

		p.db().put_cf(&cf("global"), b"s", b"x").unwrap();
		// Log data is carried in the WAL but consumes no sequence.
		let mut batch = WriteBatch::default();
		batch.put_log_data(b"note");
		batch.delete_cf(&cf("global"), b"s");
		batch.put_cf(&cf("global"), b"g", b"after");
		p.write(&batch);

		p
	}

	#[test]
	fn batch_and_cursor_are_one_write() {
		let p = primary();
		let r = TestDb::new("replica-one");
		let wal = p.wal(0);
		let (seq, count, data) = &wal[2];
		assert_eq!(*count, 4);
		let cursor = seq + count - 1;

		let before = r.seq();
		let mut group = RawBatchGroup::new();
		assert!(group.try_append(data).unwrap());
		r.apply(group, Some(cursor));

		assert_eq!(r.cursor(), Some(cursor));
		assert_eq!(r.seq(), before + count + 1);

		// One batch in the replica's WAL holds the data and the cursor.
		let applied = r.wal(before);
		assert_eq!(applied.len(), 1);
		assert_eq!(applied[0].1, count + 1);

		let cf = |name| r.db().cf_handle(name).unwrap();
		let get = |name, key: &[u8]| r.db().get_cf(&cf(name), key).unwrap();
		assert_eq!(get("pduid_pdu", b"a").as_deref(), Some(&b"overwritten"[..]));
		assert_eq!(get("pduid_pdu", b"b").as_deref(), Some(&b"2"[..]));
		assert_eq!(get("pduid_pdu", b"c").as_deref(), Some(&b"3"[..]));
		assert_eq!(get("global", b"h").as_deref(), Some(&b"2"[..]));
	}

	#[test]
	fn empty_group_with_cursor_writes_only_the_cursor() {
		let r = TestDb::new("replica-empty");
		let before = r.seq();
		r.apply(RawBatchGroup::new(), Some(42));
		assert_eq!(r.cursor(), Some(42));
		assert_eq!(r.seq(), before + 1);
	}

	#[test]
	fn group_matches_batches_applied_one_by_one() {
		let p = primary();
		let wal = p.wal(0);
		assert!(wal.len() >= 8);

		let single = TestDb::new("replica-single");
		for (_, _, data) in &wal {
			single.write(&WriteBatch::from_data(data));
		}

		let grouped = TestDb::new("replica-grouped");
		let before = grouped.seq();
		let mut group = RawBatchGroup::new();
		for (_, _, data) in &wal {
			assert!(group.try_append(data).unwrap());
		}
		assert_eq!(group.batches(), wal.len());
		let consumed: u64 = wal.iter().map(|(_, count, _)| count).sum();
		assert_eq!(u64::from(group.count()), consumed);
		let (seq, count, _) = wal.last().unwrap();
		let cursor = seq + count - 1;
		assert_eq!(cursor, p.seq());
		grouped.apply(group, Some(cursor));

		assert_eq!(grouped.contents(), p.contents());
		assert_eq!(grouped.contents(), single.contents());
		assert_eq!(grouped.cursor(), Some(cursor));
		// Every primary operation consumed a sequence, plus one for the cursor.
		assert_eq!(grouped.seq(), before + consumed + 1);
		assert_eq!(grouped.wal(before).len(), 1);
		assert_eq!(single.seq(), consumed);
	}

	#[test]
	fn bounds_split_a_drain_into_several_writes() {
		let p = primary();
		let wal = p.wal(0);
		let r = TestDb::new("replica-split");

		let mut groups = 0;
		let mut rest = wal.as_slice();
		while !rest.is_empty() {
			let before = r.seq();
			let mut group = RawBatchGroup::with_limits(usize::MAX, 3);
			let mut taken = 0;
			let mut consumed = 0;
			for (_, count, data) in rest {
				if !group.try_append(data).unwrap() {
					break;
				}
				taken += 1;
				consumed += count;
			}
			assert!(taken > 0 && taken <= 3);
			let (seq, count, _) = &rest[taken - 1];
			let cursor = seq + count - 1;
			r.apply(group, Some(cursor));
			groups += 1;

			assert_eq!(r.cursor(), Some(cursor));
			assert_eq!(r.seq(), before + consumed + 1);
			rest = &rest[taken..];
		}

		assert_eq!(groups, wal.len().div_ceil(3));
		assert!(groups > 1);
		assert_eq!(r.contents(), p.contents());
		assert_eq!(r.cursor(), Some(p.seq()));
	}

	#[test]
	fn byte_bound_takes_an_oversized_first_batch_alone() {
		let p = primary();
		let wal = p.wal(0);
		let mut group = RawBatchGroup::with_limits(1, usize::MAX);
		assert!(group.try_append(&wal[2].2).unwrap());
		assert!(!group.try_append(&wal[3].2).unwrap());
		assert_eq!(group.batches(), 1);
	}

	#[test]
	fn malformed_batch_is_refused() {
		let mut group = RawBatchGroup::new();
		group.try_append(&[0; 11]).unwrap_err();
		assert!(group.is_empty());
	}

	#[test]
	fn count_zero_batch_consumes_no_sequence() {
		let r = TestDb::new("replica-zero");
		let before = r.seq();
		let mut group = RawBatchGroup::new();
		assert!(group.try_append(&[0; HEADER_LEN]).unwrap());
		r.apply(group, None);
		assert_eq!(r.seq(), before);
		assert_eq!(r.cursor(), None);
	}
}
