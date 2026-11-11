//! Evidence for a former primary to rejoin as a replica without a full
//! checkpoint ("fast rejoin").
//!
//! A replica normally resumes from a cursor into its primary's WAL. A former
//! primary has no such cursor — its own sequence numbers mean nothing in the
//! new primary's WAL — so it has always restored a full checkpoint. That is
//! the safe answer, but it is also what a planned reboot of the primary costs:
//! tens of GB copied back to a node that almost certainly lost nothing.
//!
//! Fast rejoin replaces "always copy" with "copy unless it is proven
//! unnecessary". Call the old primary O and the node promoted in its place N.
//! Four facts, each recorded at a different moment, must all hold:
//!
//! 1. **N was an exact copy of O** (`Purity`). N holds a marker naming O's
//!    database from the moment it restored O's checkpoint until the first time
//!    it writes anything of its own that is not node-local. So at promotion,
//!    marker present means N's data is exactly what O's WAL delivered.
//! 2. **How far N got** (`Handoff`). At promotion N records O's instance id,
//!    the last sequence of O's WAL it had applied (`from_applied_seq`), and
//!    its own last sequence at that moment (`resume_after`). Only written if
//!    (1) holds.
//! 3. **O stopped cleanly and is untouched** (`CleanStop`). The last thing a
//!    clean shutdown does is record the database's final sequence `L`. It is
//!    consumed at the next open and accepted only if the database still ends
//!    at exactly `L`.
//! 4. **Nothing but bookkeeping in the gap.** Every write O made after
//!    `from_applied_seq`, up to `L` — the ones N never received — is read back
//!    from O's own WAL and must touch only node-local keys (`is_node_local`).
//!    The range must be complete: a missing sequence fails it.
//!
//! (1) and (2) give "N's data = O's data as of `from_applied_seq`". (3) and (4)
//! give "O's data did not change after that". So the two databases held the
//! same data when N took over, and O can stream N's WAL from `resume_after`.
//! Anything missing, unreadable or mismatched means a full checkpoint, exactly
//! as before — there is no partial credit.
//!
//! All of this rests on replication cursors being exact: "the last sequence
//! applied", with nothing skipped before it. See `WalFrame::resume_seq`,
//! `Engine::create_checkpoint` and the stream checks in `replication_wal` and
//! the replication worker.
//!
//! All four records are files inside the RocksDB directory. RocksDB ignores
//! them, a checkpoint never carries them, and a restore that replaces the
//! directory removes them with it, so none can outlive the database it
//! describes or follow a copy of it to another node.

use std::{
	collections::HashMap,
	fs,
	io::Write,
	path::{Path, PathBuf},
	sync::{
		Mutex,
		atomic::{AtomicBool, Ordering},
	},
};

use rocksdb::{WriteBatch, WriteBatchIteratorCf};
use tuwunel_core::{Result, err, utils::rand};

use crate::{Database, Engine, util::map_err};

/// Names this database instance. A database restored from another node's
/// checkpoint gets a fresh id the first time it is opened, and so does a wiped
/// one: it identifies "this copy of the data", which is what the proof has to
/// compare. A node address survives a wipe and does not survive a port change;
/// neither is the right thing.
const INSTANCE_FILE: &str = "TUWUNEL_INSTANCE";
const CLEAN_STOP_FILE: &str = "TUWUNEL_CLEAN_STOP";
const HANDOFF_FILE: &str = "TUWUNEL_HANDOFF";
const PURE_FILE: &str = "TUWUNEL_PURE_REPLICA";
/// Present once this database is known to hold a complete copy of its
/// primary's data as of some point under exact replication: it was restored
/// from a checkpoint of an identified primary, or proven equal by fast rejoin.
/// A replica without it ran a build whose stream dropped batches. Unlike the
/// exact-copy marker it is never dropped — local writes do not undo it.
const SYNCED_FILE: &str = "TUWUNEL_EXACT_SYNC";
/// The OS boot this database was last opened under. See `open_identity`.
const BOOT_FILE: &str = "TUWUNEL_BOOT";

/// Proof that this database stopped cleanly at a known sequence and has not
/// been written since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanStop {
	/// Last WAL sequence the database had issued when it stopped.
	pub last_seq: u64,
}

/// What a newly promoted primary knew about the primary it replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
	/// Instance id of the database this node was replicating from.
	pub from_instance: String,
	/// The last sequence of that database's WAL applied here: this node held
	/// every write up to and including it.
	pub from_applied_seq: u64,
	/// This node's own last WAL sequence when it took over. Everything it has
	/// written as primary comes after it, so it is the resume cursor for the
	/// replaced primary if it rejoins.
	pub resume_after: u64,
	/// Instance id of this (the promoted) database.
	pub instance: String,
}

impl Handoff {
	#[must_use]
	pub fn encode(&self) -> String {
		format!(
			"{} {} {} {}\n",
			self.from_instance, self.from_applied_seq, self.resume_after, self.instance
		)
	}

	pub fn decode(s: &str) -> Result<Self> {
		let mut it = s.split_whitespace();
		let mut next = |what: &str| {
			it.next()
				.ok_or_else(|| err!(Database("handoff record: missing {what}")))
		};
		let from_instance = next("from_instance")?.to_owned();
		let from_applied_seq = next("from_applied_seq")?
			.parse()
			.map_err(|e| err!(Database("handoff record: from_applied_seq: {e}")))?;
		let resume_after = next("resume_after")?
			.parse()
			.map_err(|e| err!(Database("handoff record: resume_after: {e}")))?;
		let instance = next("instance")?.to_owned();
		Ok(Self { from_instance, from_applied_seq, resume_after, instance })
	}
}

/// Outcome of checking the writes the new primary never received.
#[derive(Debug, PartialEq, Eq)]
pub enum TailCheck {
	/// Every write in the range touched only node-local keys (or there were
	/// none).
	NodeLocalOnly { writes: usize },
	/// A write the new primary does not have. `column`/`key` say which, for
	/// the log; the key is truncated and hex-encoded.
	DataWrite { column: String, key: String },
	/// The range could not be read completely from this node's WAL.
	Unreadable(String),
}

/// The database state fast rejoin needs, captured as the database opens and
/// before any service has written to it.
pub(crate) struct RejoinState {
	pub(crate) instance: String,
	pub(crate) clean_stop: Option<CleanStop>,
	/// RocksDB column family id → name, for reading WAL batches.
	pub(crate) column_names: HashMap<u32, String>,
}

impl RejoinState {
	/// Must run immediately after the engine opens: the clean-stop check
	/// compares the record with the database's sequence *before* startup
	/// writes anything.
	pub(crate) fn load(engine: &Engine, maps: &crate::maps::Maps) -> Result<Self> {
		let db_path = engine.ctx.server.config.database_path.clone();
		let column_names = column_names(maps);

		// A database opened read-only is not this node's replication copy and
		// its directory may not be writable. Give it an identity that matches
		// nothing and leave the records alone: every proof then fails closed.
		if engine.is_read_only() || engine.is_secondary() {
			return Ok(Self {
				instance: format!("readonly-{}", rand::string(24)),
				clean_stop: None,
				column_names,
			});
		}

		let (instance, clean_stop) =
			open_identity(&db_path, engine.latest_wal_sequence(), current_boot().as_deref())?;

		Ok(Self { instance, clean_stop, column_names })
	}
}

/// Settle, at open, who this database is and what the previous run left
/// behind. Returns the instance id and the previous run's clean stop, if it
/// was proven.
///
/// Everything fast rejoin trusts about a database is only as good as the last
/// time someone vouched for where it ended. So when the previous run did not
/// end in a proven clean stop:
///
/// - The exact-copy marker and the handoff are dropped. Both were statements
///   about this database made by a run that can no longer be accounted for —
///   it crashed, or a build that knows nothing of these records ran in
///   between and wrote past them.
/// - If the machine has also rebooted since (or that cannot be told), the
///   **instance id is replaced**. Writes reach the WAL file without an fsync,
///   and the replication stream serves them from there; a process crash loses
///   none of that, but a power loss or kernel crash can drop the tail — after
///   a replica has already applied it. The database then reuses those
///   sequence numbers for different writes, and a replica resuming by
///   sequence would splice the two histories with nothing to show for it. A
///   new id says "not the database your cursor came from": replicas re-sync
///   from a checkpoint instead.
///
/// A directory with no instance file is new — just created, or just restored
/// from a checkpoint — and has no previous run to account for; a marker
/// written by the restore is kept.
pub(crate) fn open_identity(
	db_path: &Path,
	opened_seq: u64,
	boot_now: Option<&str>,
) -> Result<(String, Option<CleanStop>)> {
	let existing = read_trimmed(&db_path.join(INSTANCE_FILE))
		.map_err(|e| err!(Database("Reading {INSTANCE_FILE}: {e}")))?;
	let boot_prev = read_trimmed(&db_path.join(BOOT_FILE)).ok().flatten();

	let (instance, clean_stop) = match existing {
		| None => {
			let _: std::io::Result<()> = fs::remove_file(db_path.join(CLEAN_STOP_FILE));
			(new_instance(db_path)?, None)
		},
		| Some(id) => match take_clean_stop(db_path, &id, opened_seq) {
			| Some(stop) => (id, Some(stop)),
			| None => {
				for name in [PURE_FILE, HANDOFF_FILE] {
					let _: std::io::Result<()> = fs::remove_file(db_path.join(name));
				}
				sync_dir(&db_path.join(INSTANCE_FILE));

				let same_boot =
					matches!((boot_now, boot_prev.as_deref()), (Some(a), Some(b)) if a == b);
				if same_boot {
					(id, None)
				} else {
					let new = new_instance(db_path)?;
					tuwunel_core::warn!(
						old = %id,
						new = %new,
						"The previous run did not end in a clean stop and the machine has \
						 restarted since (or that cannot be determined): this database may \
						 have lost writes its replicas already hold. It now has a new \
						 instance id; replicas will re-sync from a checkpoint."
					);
					(new, None)
				}
			},
		},
	};

	// Best effort: without it the next unclean stop is treated as a reboot.
	match boot_now {
		| Some(boot) => {
			let _: std::io::Result<()> = write_synced(&db_path.join(BOOT_FILE), boot.as_bytes());
		},
		| None => {
			let _: std::io::Result<()> = fs::remove_file(db_path.join(BOOT_FILE));
		},
	}

	Ok((instance, clean_stop))
}

fn new_instance(db_path: &Path) -> Result<String> {
	let id = rand::string(32);
	write_synced(&db_path.join(INSTANCE_FILE), id.as_bytes())
		.map_err(|e| err!(Database("Writing {INSTANCE_FILE}: {e}")))?;
	Ok(id)
}

/// `Ok(None)` for a missing or empty file.
fn read_trimmed(path: &Path) -> std::io::Result<Option<String>> {
	match fs::read_to_string(path) {
		| Ok(s) => {
			let s = s.trim();
			Ok((!s.is_empty()).then(|| s.to_owned()))
		},
		| Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
		| Err(e) => Err(e),
	}
}

/// Identifies the running OS boot. Linux only; `None` elsewhere, which
/// `open_identity` treats as "cannot tell", the cautious reading.
fn current_boot() -> Option<String> {
	read_trimmed(Path::new("/proc/sys/kernel/random/boot_id"))
		.ok()
		.flatten()
}

/// Read and delete the clean-stop record. Deleted unconditionally — it speaks
/// for the previous run only, and this run will write to the database before
/// it ends.
fn take_clean_stop(db_path: &Path, instance: &str, opened_seq: u64) -> Option<CleanStop> {
	let path = db_path.join(CLEAN_STOP_FILE);
	let content = fs::read_to_string(&path).ok();
	// Synced: a record that came back after a power loss would vouch for a run
	// it never saw.
	if fs::remove_file(&path).is_ok() {
		sync_dir(&path);
	}

	let content = content?;
	let mut it = content.split_whitespace();
	let (Some(rec_instance), Some(rec_seq)) = (it.next(), it.next()) else {
		tuwunel_core::warn!("Ignoring malformed {CLEAN_STOP_FILE}");
		return None;
	};
	let last_seq: u64 = rec_seq.parse().ok()?;

	if rec_instance != instance {
		tuwunel_core::warn!(
			"Ignoring {CLEAN_STOP_FILE}: it belongs to instance {rec_instance}, this database \
			 is {instance}"
		);
		return None;
	}
	if opened_seq != last_seq {
		tuwunel_core::warn!(
			opened_seq,
			last_seq,
			"Ignoring {CLEAN_STOP_FILE}: the database does not end where the clean stop said \
			 it did"
		);
		return None;
	}

	Some(CleanStop { last_seq })
}

/// Column family ids are assigned by RocksDB and not exposed by the bindings.
/// A batch iterated with `iterate_cf` reports the id of each column it
/// touches, so put one key per column into a throwaway batch (never written)
/// and read the ids back.
fn column_names(maps: &crate::maps::Maps) -> HashMap<u32, String> {
	struct Ids(Vec<u32>);
	impl WriteBatchIteratorCf for Ids {
		fn put_cf(&mut self, cf_id: u32, _: &[u8], _: &[u8]) { self.0.push(cf_id); }

		fn delete_cf(&mut self, _: u32, _: &[u8]) {}

		fn merge_cf(&mut self, _: u32, _: &[u8], _: &[u8]) {}
	}

	let names: Vec<&str> = maps.keys().copied().collect();
	// `put_cf` is called once per name, in order, so ids come back in the same
	// order as `names`.
	let mut batch = WriteBatch::default();
	for name in &names {
		if let Some(map) = maps.get(name) {
			batch.put_cf(&map.cf(), b"", b"");
		}
	}
	let mut ids = Ids(Vec::with_capacity(names.len()));
	batch.iterate_cf(&mut ids);

	ids.0
		.into_iter()
		.zip(names)
		.map(|(id, name)| (id, name.to_owned()))
		.collect()
}

/// Writes that belong to this node rather than to the replicated data set.
/// The new primary not having them is not a loss.
///
/// - `replication_meta`: this node's own replication cursor and bookkeeping.
/// - `global` / `clean_c`: the clean-shutdown counter mark, written as the very
///   last step of every clean stop — after the WAL stream has closed, so a
///   replica cannot have it by the time the primary is down — and removed
///   again on the next start. It describes this node's own shutdown only.
///
/// Anything else, however harmless it looks, is treated as data. Extending this
/// list is a correctness decision: an entry here is a write the cluster agrees
/// to lose.
#[must_use]
pub fn is_node_local(column: &str, key: &[u8]) -> bool {
	match column {
		| "replication_meta" => true,
		| "global" => key == b"clean_c",
		| _ => false,
	}
}

impl Database {
	/// This database's instance id. See `INSTANCE_FILE`.
	#[must_use]
	pub fn instance_id(&self) -> &str { &self.rejoin.instance }

	/// The previous run's clean stop, if it was proven at open.
	#[must_use]
	pub fn proven_clean_stop(&self) -> Option<CleanStop> { self.rejoin.clean_stop }

	/// Record a clean stop. Call last, once nothing can write to the database:
	/// the record is only honoured if the database still ends here when it is
	/// next opened.
	pub fn record_clean_stop(&self) -> Result {
		if self.engine.is_read_only() || self.engine.is_secondary() {
			return Ok(());
		}
		self.engine.sync()?;
		let seq = self.engine.latest_wal_sequence();
		let db_path = &self.engine.ctx.server.config.database_path;
		write_synced(
			&db_path.join(CLEAN_STOP_FILE),
			format!("{} {seq}\n", self.rejoin.instance).as_bytes(),
		)
		.map_err(|e| err!(Database("Writing {CLEAN_STOP_FILE}: {e}")))
	}

	/// The primary instance this database is still an exact copy of, if any.
	/// See `Purity`.
	#[must_use]
	pub fn pure_replica_of(&self) -> Option<String> { self.engine.purity.pure_of() }

	/// Record that this database has been proven equal to `primary_instance`.
	/// `Ok(false)` if this process has already made a local data write.
	pub fn assert_pure_replica_of(&self, primary_instance: &str) -> Result<bool> {
		self.engine.purity.assert_pure(primary_instance)
	}

	/// This database can no longer be shown to be a copy of its primary.
	pub fn revoke_pure_replica(&self) { self.engine.purity.revoke(); }

	/// Whether this database has been synced under exact replication. See
	/// `SYNCED_FILE`.
	#[must_use]
	pub fn exact_synced(&self) -> bool {
		self.engine
			.ctx
			.server
			.config
			.database_path
			.join(SYNCED_FILE)
			.exists()
	}

	/// Record that this database holds a complete copy of `primary_instance`.
	pub fn mark_exact_synced(&self, primary_instance: &str) -> Result {
		let path = self
			.engine
			.ctx
			.server
			.config
			.database_path
			.join(SYNCED_FILE);
		write_synced(&path, primary_instance.as_bytes())
			.map_err(|e| err!(Database("Writing {SYNCED_FILE}: {e}")))
	}

	/// Record, at promotion, what this node knew about the primary it replaces.
	///
	/// The WAL is synced first. The handoff vouches for every batch applied up
	/// to `from_applied_seq`, and it is fsynced; if those batches were still
	/// only in the page cache, a power loss would keep the claim and lose what
	/// it claims.
	pub fn write_handoff(&self, handoff: &Handoff) -> Result {
		self.engine.sync()?;
		write_synced(&self.handoff_path(), handoff.encode().as_bytes())
			.map_err(|e| err!(Database("Writing {HANDOFF_FILE}: {e}")))
	}

	/// The last promotion's handoff, if this node was promoted and has not
	/// since become a replica.
	pub fn read_handoff(&self) -> Result<Option<Handoff>> {
		match fs::read_to_string(self.handoff_path()) {
			| Ok(s) => Handoff::decode(&s).map(Some),
			| Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
			| Err(e) => Err(err!(Database("Reading {HANDOFF_FILE}: {e}"))),
		}
	}

	/// Forget the handoff: this node is no longer the primary it describes.
	pub fn clear_handoff(&self) {
		let _: std::io::Result<()> = fs::remove_file(self.handoff_path());
	}

	/// Check that every write this database made after sequence `after`, up to
	/// and including `through`, is node-local. `after >= through` is an empty
	/// range.
	pub fn check_wal_tail(&self, after: u64, through: u64) -> TailCheck {
		if after >= through {
			return TailCheck::NodeLocalOnly { writes: 0 };
		}

		// Yields the batches that start after `after`.
		let iter = match self.engine.wal_updates_since(after) {
			| Ok(iter) => iter,
			| Err(e) => return TailCheck::Unreadable(format!("WAL after {after}: {e}")),
		};

		check_batches(
			iter.map(|r| r.map_err(map_err)),
			after,
			through,
			&self.rejoin.column_names,
		)
	}

	fn handoff_path(&self) -> PathBuf {
		self.engine
			.ctx
			.server
			.config
			.database_path
			.join(HANDOFF_FILE)
	}
}

/// Classify the WAL batches covering the sequences after `after`, up to and
/// including `through`. Every sequence in the range must be accounted for: a
/// batch that does not start directly after the last one means part of the
/// range is missing, and a range that ends early means the WAL was truncated —
/// both are `Unreadable`, never a pass.
pub(crate) fn check_batches<I>(
	batches: I,
	after: u64,
	through: u64,
	column_names: &HashMap<u32, String>,
) -> TailCheck
where
	I: Iterator<Item = Result<(u64, WriteBatch)>>,
{
	struct Visit<'a> {
		names: &'a HashMap<u32, String>,
		writes: usize,
		first_data: Option<(String, String)>,
	}

	impl Visit<'_> {
		fn saw(&mut self, cf_id: u32, key: &[u8]) {
			self.writes = self.writes.saturating_add(1);
			if self.first_data.is_some() {
				return;
			}
			let column = self
				.names
				.get(&cf_id)
				.map_or_else(|| format!("<column {cf_id}>"), Clone::clone);
			if !is_node_local(&column, key) {
				self.first_data = Some((column, hex(key)));
			}
		}
	}

	impl WriteBatchIteratorCf for Visit<'_> {
		fn put_cf(&mut self, cf_id: u32, key: &[u8], _: &[u8]) { self.saw(cf_id, key); }

		fn delete_cf(&mut self, cf_id: u32, key: &[u8]) { self.saw(cf_id, key); }

		fn merge_cf(&mut self, cf_id: u32, key: &[u8], _: &[u8]) { self.saw(cf_id, key); }
	}

	if after >= through {
		return TailCheck::NodeLocalOnly { writes: 0 };
	}

	let mut visit = Visit { names: column_names, writes: 0, first_data: None };
	// Last sequence accounted for so far.
	let mut covered = after;

	for item in batches {
		let (seq, batch) = match item {
			| Ok(item) => item,
			| Err(e) => return TailCheck::Unreadable(format!("WAL read: {e}")),
		};

		// The batch occupies `seq ..= last`. One that consumed no sequences
		// (`count == 0`) has `last == seq - 1` and moves nothing.
		let count = u64::try_from(batch.len()).unwrap_or(u64::MAX);
		let last = seq.saturating_add(count).saturating_sub(1);

		// Entirely at or before `after`: the new primary has it. (The WAL
		// iterator does not yield these; this keeps the check right if it
		// ever does.)
		if count > 0 && last <= covered {
			continue;
		}
		if seq > covered.saturating_add(1) {
			return TailCheck::Unreadable(format!(
				"WAL skips from {covered} to {seq}"
			));
		}

		// A batch that starts at or before `covered` but runs past it is
		// inspected whole, which can only make the check stricter.
		let seen_before = visit.writes;
		batch.iterate_cf(&mut visit);
		if let Some((column, key)) = visit.first_data.take() {
			return TailCheck::DataWrite { column, key };
		}

		// `iterate_cf` reports puts, deletes and merges. A batch holding any
		// other operation (a range delete, say) would otherwise pass with
		// that operation never looked at.
		let seen = visit.writes.saturating_sub(seen_before);
		if u64::try_from(seen).unwrap_or(u64::MAX) != count {
			return TailCheck::Unreadable(format!(
				"batch at seq {seq} holds {count} operations but only {seen} could be read"
			));
		}

		covered = covered.max(last);
		if covered >= through {
			return TailCheck::NodeLocalOnly { writes: visit.writes };
		}
	}

	TailCheck::Unreadable(format!("WAL ends at {covered} before reaching {through}"))
}

/// Write via a temp file and rename, fsyncing both, so a crash never leaves a
/// truncated record that could parse as a valid one.
fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
	// Not `.tmp`/`.dbtmp`: nothing RocksDB could mistake for one of its own.
	let mut tmp = path.as_os_str().to_owned();
	tmp.push(".new");
	let tmp = PathBuf::from(tmp);
	{
		let mut f = fs::File::create(&tmp)?;
		f.write_all(data)?;
		f.sync_all()?;
	}
	fs::rename(&tmp, path)?;
	if let Some(dir) = path.parent() {
		fs::File::open(dir)?.sync_all()?;
	}
	Ok(())
}

/// Tracks whether this database is still an exact copy of its primary.
///
/// The marker file names the primary instance this database was restored
/// from (or proven equal to). It is removed, durably, *before* the first local
/// write that is not node-local goes to RocksDB — so if such a write exists in
/// the database, the marker does not, whatever crashes in between.
///
/// Writes applied from the primary's WAL do not pass through here: they are
/// the copy, not a departure from it.
pub(crate) struct Purity {
	marker: PathBuf,
	/// Set once this process has made a local data write. Checked first on
	/// every write, so a primary pays one atomic load.
	data_write: AtomicBool,
	/// Serialises marker changes against the first data write.
	lock: Mutex<()>,
}

impl Purity {
	pub(crate) fn new(db_path: &Path) -> Self {
		Self {
			marker: db_path.join(PURE_FILE),
			data_write: AtomicBool::new(false),
			lock: Mutex::new(()),
		}
	}

	/// Call before every local write, with the column and key about to be
	/// written.
	#[inline]
	pub(crate) fn note_write(&self, column: &str, key: &[u8]) {
		if self.data_write.load(Ordering::Acquire) || is_node_local(column, key) {
			return;
		}
		self.first_data_write(column, key);
	}

	#[cold]
	fn first_data_write(&self, column: &str, key: &[u8]) {
		let _guard = self.lock.lock().expect("purity lock poisoned");
		if self.data_write.load(Ordering::Acquire) {
			return;
		}

		match fs::remove_file(&self.marker) {
			| Ok(()) => {
				sync_dir(&self.marker);
				tuwunel_core::info!(
					column,
					key = %hex(key),
					"First local data write: this database is no longer an exact copy of its \
					 primary. If this node is promoted, the node it replaces will need a full \
					 checkpoint to rejoin."
				);
			},
			| Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
			// The write is about to happen and the marker would then be a lie
			// that survives a restart. Database write errors are fatal in this
			// codebase (`expect("database insert error")`); so is this.
			| Err(e) => panic!("cannot remove {PURE_FILE} before a local write: {e}"),
		}

		self.data_write.store(true, Ordering::Release);
	}

	/// The primary instance this database is still an exact copy of, if any.
	pub(crate) fn pure_of(&self) -> Option<String> {
		let _guard = self.lock.lock().expect("purity lock poisoned");
		if self.data_write.load(Ordering::Acquire) {
			return None;
		}
		let s = fs::read_to_string(&self.marker).ok()?;
		let s = s.trim();
		(!s.is_empty()).then(|| s.to_owned())
	}

	/// Record that this database is an exact copy of `primary_instance`.
	/// Refused (`Ok(false)`) if this process has already made a local data
	/// write.
	pub(crate) fn assert_pure(&self, primary_instance: &str) -> Result<bool> {
		let _guard = self.lock.lock().expect("purity lock poisoned");
		if self.data_write.load(Ordering::Acquire) {
			return Ok(false);
		}
		write_synced(&self.marker, primary_instance.as_bytes())
			.map_err(|e| err!(Database("Writing {PURE_FILE}: {e}")))?;
		Ok(true)
	}

	/// Drop the marker: the primary is not the database it names.
	pub(crate) fn revoke(&self) {
		let _guard = self.lock.lock().expect("purity lock poisoned");
		if fs::remove_file(&self.marker).is_ok() {
			sync_dir(&self.marker);
		}
	}
}

/// Mark a freshly restored checkpoint as an exact copy of `primary_instance`.
/// For the pre-open bootstrap path, which runs before there is a `Database`.
pub fn mark_restored_checkpoint(db_path: &Path, primary_instance: &str) -> Result {
	write_synced(&db_path.join(SYNCED_FILE), primary_instance.as_bytes())
		.map_err(|e| err!(Database("Writing {SYNCED_FILE}: {e}")))?;
	write_synced(&db_path.join(PURE_FILE), primary_instance.as_bytes())
		.map_err(|e| err!(Database("Writing {PURE_FILE}: {e}")))
}

fn sync_dir(file: &Path) {
	if let Some(dir) = file.parent()
		&& let Ok(dir) = fs::File::open(dir)
	{
		let _: std::io::Result<()> = dir.sync_all();
	}
}

fn hex(key: &[u8]) -> String {
	key[..key.len().min(32)]
		.iter()
		.map(|b| format!("{b:02x}"))
		.collect()
}

#[cfg(test)]
mod tests {
	use rocksdb::{ColumnFamilyDescriptor, DB, Options, WriteBatch, checkpoint::Checkpoint};

	use super::*;

	const COLUMNS: &[&str] = &["global", "pduid_pdu", "replication_meta"];

	struct TestDb {
		dir: PathBuf,
		db: Option<DB>,
	}

	impl TestDb {
		fn new(tag: &str) -> Self {
			let dir = std::env::temp_dir().join(format!(
				"tuwunel-rejoin-test-{tag}-{}-{}",
				std::process::id(),
				rand::string(8)
			));
			let _ = fs::remove_dir_all(&dir);
			let mut this = Self { dir, db: None };
			this.open();
			this
		}

		fn open(&mut self) {
			let mut opts = Options::default();
			opts.create_if_missing(true);
			opts.create_missing_column_families(true);
			// Keep archived WAL, as the server does, so old sequences stay readable.
			opts.set_wal_ttl_seconds(3600);
			let cfs = COLUMNS
				.iter()
				.map(|name| ColumnFamilyDescriptor::new(*name, Options::default()));
			self.db = Some(DB::open_cf_descriptors(&opts, self.dir.join("data"), cfs).unwrap());
		}

		fn close(&mut self) { self.db = None; }

		fn db(&self) -> &DB { self.db.as_ref().unwrap() }

		fn data(&self) -> PathBuf { self.dir.join("data") }

		fn put(&self, column: &str, key: &[u8]) {
			self.db()
				.put_cf(&self.db().cf_handle(column).unwrap(), key, b"v")
				.unwrap();
		}

		/// One batch with a put per `(column, key)`.
		fn batch(&self, writes: &[(&str, &[u8])]) {
			let mut batch = WriteBatch::default();
			for (column, key) in writes {
				batch.put_cf(&self.db().cf_handle(column).unwrap(), key, b"v");
			}
			self.db().write(&batch).unwrap();
		}

		fn seq(&self) -> u64 { self.db().latest_sequence_number() }

		/// Column id → name, built the same way `column_names` does.
		fn names(&self) -> HashMap<u32, String> {
			struct Ids(Vec<u32>);
			impl WriteBatchIteratorCf for Ids {
				fn put_cf(&mut self, cf_id: u32, _: &[u8], _: &[u8]) { self.0.push(cf_id); }

				fn delete_cf(&mut self, _: u32, _: &[u8]) {}

				fn merge_cf(&mut self, _: u32, _: &[u8], _: &[u8]) {}
			}
			let mut batch = WriteBatch::default();
			for name in COLUMNS {
				batch.put_cf(&self.db().cf_handle(name).unwrap(), b"", b"");
			}
			let mut ids = Ids(Vec::new());
			batch.iterate_cf(&mut ids);
			assert_eq!(ids.0.len(), COLUMNS.len());
			ids.0
				.into_iter()
				.zip(COLUMNS.iter().map(|n| (*n).to_owned()))
				.collect()
		}

		/// Writes after `after`, up to and including `through`.
		fn check(&self, after: u64, through: u64) -> TailCheck {
			let iter = self.db().get_updates_since(after).unwrap();
			check_batches(iter.map(|r| r.map_err(map_err)), after, through, &self.names())
		}
	}

	impl Drop for TestDb {
		fn drop(&mut self) {
			self.db = None;
			let _ = fs::remove_dir_all(&self.dir);
		}
	}

	/// The contract every cursor in the replication code relies on:
	/// `get_updates_since(n)` yields the batches that start *after* `n`, and
	/// they are contiguous in sequence. So a resume cursor must be the last
	/// sequence consumed — a cursor naming the start of the next batch skips
	/// that batch.
	#[test]
	fn updates_since_yields_batches_starting_after_the_sequence() {
		let t = TestDb::new("since");
		t.put("pduid_pdu", b"a"); // seq 1
		t.batch(&[("pduid_pdu", b"b"), ("pduid_pdu", b"c"), ("pduid_pdu", b"d")]); // 2..=4
		t.put("pduid_pdu", b"e"); // seq 5
		assert_eq!(t.seq(), 5);

		let first = |since: u64| {
			t.db()
				.get_updates_since(since)
				.unwrap()
				.next()
				.map(|r| r.unwrap())
				.map(|(seq, batch)| (seq, batch.len()))
		};
		assert_eq!(first(0), Some((1, 1)));
		assert_eq!(first(1), Some((2, 3)), "the batch starting at `since` is not yielded");
		assert_eq!(first(4), Some((5, 1)), "last sequence of a batch: next batch is yielded");
		assert_eq!(first(5), None, "caught up");

		let mut covered = 0;
		for item in t.db().get_updates_since(0).unwrap() {
			let (seq, batch) = item.unwrap();
			assert_eq!(seq, covered + 1, "batches are contiguous");
			covered = seq + batch.len() as u64 - 1;
		}
		assert_eq!(covered, 5);
	}

	/// The primary's stream loop, as `replication_wal` runs it: drain from the
	/// cursor, advance the cursor with `WalFrame::resume_seq`, repeat. Every
	/// batch must arrive exactly once, across polls and across a reconnect
	/// that resumes from the secondary's saved cursor.
	///
	/// With the cursor at `sequence + count` this delivered 11 of 15 writes:
	/// the first batch of every poll was skipped.
	#[test]
	fn stream_cursor_delivers_every_batch_exactly_once() {
		use crate::WalFrame;

		let t = TestDb::new("stream");
		t.put("pduid_pdu", b"before"); // in the checkpoint
		let mut cursor = t.seq(); // what create_checkpoint hands the secondary

		let mut delivered: Vec<u64> = Vec::new();
		let mut poll = |cursor: &mut u64, delivered: &mut Vec<u64>| {
			for item in t.db().get_updates_since(*cursor).unwrap() {
				let (seq, batch) = item.unwrap();
				let frame = WalFrame::data(seq, batch.len() as u64, batch.data().to_vec());
				assert_eq!(frame.sequence, *cursor + 1, "frame is the batch after the cursor");
				for s in seq..seq + batch.len() as u64 {
					delivered.push(s);
				}
				*cursor = frame.resume_seq();
			}
		};

		let mut written = 0;
		for round in 0..6u32 {
			// Mixed single puts and multi-key batches, like real traffic.
			t.put("pduid_pdu", format!("{round}-a").as_bytes());
			t.batch(&[("pduid_pdu", b"x"), ("global", b"c"), ("pduid_pdu", b"y")]);
			t.put("pduid_pdu", format!("{round}-b").as_bytes());
			written += 5;

			if round == 3 {
				// Reconnect: a fresh stream from the secondary's saved cursor.
				let saved = cursor;
				cursor = saved;
			}
			poll(&mut cursor, &mut delivered);
			// A heartbeat carries the cursor and must not move it.
			assert_eq!(WalFrame::heartbeat(cursor).resume_seq(), cursor);
			// An idle poll delivers nothing and leaves the cursor alone.
			let before = cursor;
			poll(&mut cursor, &mut delivered);
			assert_eq!(cursor, before);
		}

		let expected: Vec<u64> = (2..2 + written).collect();
		assert_eq!(delivered, expected, "every sequence once, in order");
		assert_eq!(cursor, t.seq());
	}

	#[test]
	fn tail_of_bookkeeping_only_passes() {
		let t = TestDb::new("local");
		t.put("pduid_pdu", b"delivered");
		let after = t.seq();
		t.put("global", b"clean_c");
		t.batch(&[("replication_meta", b"primary_resume_seq"), ("replication_meta", b"x")]);
		assert_eq!(t.check(after, t.seq()), TailCheck::NodeLocalOnly { writes: 3 });
	}

	#[test]
	fn empty_tail_passes() {
		let t = TestDb::new("empty");
		t.put("pduid_pdu", b"delivered");
		let last = t.seq();
		// The new primary had applied the very last write.
		assert_eq!(t.check(last, last), TailCheck::NodeLocalOnly { writes: 0 });
	}

	#[test]
	fn one_data_write_in_the_tail_fails() {
		let t = TestDb::new("data");
		t.put("pduid_pdu", b"delivered");
		let after = t.seq();
		t.put("global", b"clean_c");
		t.put("pduid_pdu", b"lost");
		t.put("global", b"clean_c");
		assert_eq!(t.check(after, t.seq()), TailCheck::DataWrite {
			column: "pduid_pdu".into(),
			key: hex(b"lost"),
		});
	}

	/// The same column can hold both kinds of key: only the exact bookkeeping
	/// key is exempt.
	#[test]
	fn other_keys_in_a_bookkeeping_column_fail() {
		let t = TestDb::new("global");
		let after = t.seq();
		t.put("global", b"c");
		assert!(matches!(t.check(after, t.seq()), TailCheck::DataWrite { .. }));
	}

	/// A data write hidden inside a batch that also holds bookkeeping.
	#[test]
	fn mixed_batch_fails() {
		let t = TestDb::new("mixed");
		let after = t.seq();
		t.batch(&[("global", b"clean_c"), ("pduid_pdu", b"lost"), ("replication_meta", b"x")]);
		assert!(matches!(t.check(after, t.seq()), TailCheck::DataWrite { .. }));
	}

	/// A delete is a write too.
	#[test]
	fn data_delete_in_the_tail_fails() {
		let t = TestDb::new("delete");
		t.put("pduid_pdu", b"k");
		let after = t.seq();
		t.db()
			.delete_cf(&t.db().cf_handle("pduid_pdu").unwrap(), b"k")
			.unwrap();
		assert!(matches!(t.check(after, t.seq()), TailCheck::DataWrite { .. }));
	}

	/// A cursor that falls mid-batch cannot vouch for the rest of that batch.
	/// The iterator does not yield it, so the range starts with a hole.
	#[test]
	fn cursor_inside_a_batch_is_unreadable() {
		let t = TestDb::new("straddle");
		t.batch(&[("global", b"clean_c"), ("pduid_pdu", b"a"), ("global", b"clean_c")]); // 1..=3
		t.put("global", b"clean_c"); // 4
		assert!(matches!(t.check(1, 4), TailCheck::Unreadable(_)));
	}

	/// If batches at or before the cursor are ever yielded, they are skipped
	/// rather than failing the check, and one that runs past the cursor is
	/// inspected whole.
	#[test]
	fn batches_before_the_cursor_are_skipped_and_straddlers_inspected() {
		let t = TestDb::new("inclusive");
		t.put("pduid_pdu", b"delivered"); // 1
		t.batch(&[("global", b"clean_c"), ("pduid_pdu", b"lost")]); // 2..=3
		t.put("global", b"clean_c"); // 4
		let all = || t.db().get_updates_since(0).unwrap().map(|r| r.map_err(map_err));

		// Cursor 3: batches 1 and 2..=3 are behind it, only 4 is checked.
		assert_eq!(check_batches(all(), 3, 4, &t.names()), TailCheck::NodeLocalOnly { writes: 1 });
		// Cursor 2 is inside the 2..=3 batch: inspected whole, data found.
		assert!(matches!(check_batches(all(), 2, 4, &t.names()), TailCheck::DataWrite { .. }));
	}

	/// The range must be covered to its end. A WAL that stops short proves
	/// nothing about the sequences it does not hold.
	#[test]
	fn tail_that_stops_short_is_unreadable() {
		let t = TestDb::new("short");
		let after = t.seq();
		t.put("global", b"clean_c");
		let claimed_end = t.seq() + 5;
		assert!(matches!(t.check(after, claimed_end), TailCheck::Unreadable(_)));
	}

	/// A hole in the middle of the range is not a pass either.
	#[test]
	fn gap_in_the_tail_is_unreadable() {
		let t = TestDb::new("gap");
		t.put("global", b"clean_c"); // seq 1
		t.put("global", b"clean_c"); // seq 2
		t.put("global", b"clean_c"); // seq 3
		let all: Vec<_> = t
			.db()
			.get_updates_since(0)
			.unwrap()
			.map(|r| r.map_err(map_err))
			.collect();
		assert_eq!(all.len(), 3);
		let holed = all.into_iter().enumerate().filter(|(i, _)| *i != 1).map(|(_, b)| b);
		assert!(matches!(check_batches(holed, 0, 3, &t.names()), TailCheck::Unreadable(_)));
	}

	/// A column this build does not know is data, not bookkeeping.
	#[test]
	fn unknown_column_fails() {
		let t = TestDb::new("unknown");
		let after = t.seq();
		t.put("replication_meta", b"x");
		let iter = t.db().get_updates_since(after).unwrap();
		let got = check_batches(iter.map(|r| r.map_err(map_err)), after, t.seq(), &HashMap::new());
		assert!(matches!(got, TailCheck::DataWrite { .. }));
	}

	/// The clean-stop record is honoured only for this instance, only while
	/// the database still ends where it said, and only once.
	#[test]
	fn clean_stop_is_proven_only_for_an_untouched_database() {
		let mut t = TestDb::new("stop");
		t.put("pduid_pdu", b"a");
		let last = t.seq();
		let record = |t: &TestDb, instance: &str, seq: u64| {
			write_synced(&t.data().join(CLEAN_STOP_FILE), format!("{instance} {seq}\n").as_bytes())
				.unwrap();
		};

		// Sequence survives a clean close and reopen unchanged.
		t.close();
		t.open();
		assert_eq!(t.seq(), last);

		record(&t, "me", last);
		assert_eq!(take_clean_stop(&t.data(), "me", t.seq()), Some(CleanStop { last_seq: last }));
		assert_eq!(take_clean_stop(&t.data(), "me", t.seq()), None, "consumed by the first read");

		record(&t, "someone-else", last);
		assert_eq!(take_clean_stop(&t.data(), "me", t.seq()), None);

		record(&t, "me", last);
		t.put("pduid_pdu", b"written after the record");
		assert_eq!(take_clean_stop(&t.data(), "me", t.seq()), None);
		assert!(!t.data().join(CLEAN_STOP_FILE).exists(), "a rejected record is still consumed");

		write_synced(&t.data().join(CLEAN_STOP_FILE), b"garbage").unwrap();
		assert_eq!(take_clean_stop(&t.data(), "me", t.seq()), None);
	}

	/// The records live in the RocksDB directory. RocksDB must leave them
	/// alone, and a checkpoint must not carry them to another node.
	#[test]
	fn records_survive_rocksdb_and_stay_out_of_checkpoints() {
		let mut t = TestDb::new("files");
		new_instance(&t.data()).unwrap();
		mark_restored_checkpoint(&t.data(), "primary").unwrap();
		write_synced(&t.data().join(HANDOFF_FILE), b"a 1 2 b\n").unwrap();

		for i in 0..2000u32 {
			t.put("pduid_pdu", &i.to_be_bytes());
		}
		t.db().flush().unwrap();
		t.db().compact_range::<&[u8], &[u8]>(None, None);
		t.close();
		t.open();

		for name in [INSTANCE_FILE, PURE_FILE, HANDOFF_FILE] {
			assert!(t.data().join(name).exists(), "{name} was removed by RocksDB");
		}

		let dest = t.dir.join("checkpoint");
		Checkpoint::new(t.db()).unwrap().create_checkpoint(&dest).unwrap();
		for name in [INSTANCE_FILE, PURE_FILE, SYNCED_FILE, HANDOFF_FILE, CLEAN_STOP_FILE] {
			assert!(!dest.join(name).exists(), "{name} leaked into a checkpoint");
		}
	}

	#[test]
	fn marker_is_dropped_before_the_first_data_write_and_stays_dropped() {
		let t = TestDb::new("purity");
		mark_restored_checkpoint(&t.data(), "primary").unwrap();
		let purity = Purity::new(&t.data());
		assert_eq!(purity.pure_of().as_deref(), Some("primary"));

		// Bookkeeping does not count.
		purity.note_write("replication_meta", b"primary_resume_seq");
		purity.note_write("global", b"clean_c");
		assert_eq!(purity.pure_of().as_deref(), Some("primary"));

		purity.note_write("pduid_pdu", b"k");
		assert_eq!(purity.pure_of(), None);
		assert!(!t.data().join(PURE_FILE).exists());

		// Once this process has written data it cannot be declared pure again.
		assert!(!purity.assert_pure("primary").unwrap());
		assert_eq!(purity.pure_of(), None);

		// And a restart does not bring the marker back.
		assert_eq!(Purity::new(&t.data()).pure_of(), None);
	}

	#[test]
	fn marker_can_be_asserted_by_a_process_with_no_data_writes() {
		let t = TestDb::new("assert");
		let purity = Purity::new(&t.data());
		assert_eq!(purity.pure_of(), None, "no marker: not known to be a copy");
		purity.note_write("replication_meta", b"x");
		assert!(purity.assert_pure("n").unwrap());
		assert_eq!(purity.pure_of().as_deref(), Some("n"));
		purity.revoke();
		assert_eq!(purity.pure_of(), None);
	}

	#[test]
	fn handoff_round_trips_and_rejects_damage() {
		let h = Handoff {
			from_instance: "old".into(),
			from_applied_seq: 411_193_222,
			resume_after: 98_765,
			instance: "new".into(),
		};
		assert_eq!(Handoff::decode(&h.encode()).unwrap(), h);
		assert!(Handoff::decode("").is_err());
		assert!(Handoff::decode("old 1 2").is_err());
		assert!(Handoff::decode("old x 2 new").is_err());
	}

	#[test]
	fn only_named_bookkeeping_keys_are_node_local() {
		assert!(is_node_local("replication_meta", b"anything"));
		assert!(is_node_local("global", b"clean_c"));
		assert!(!is_node_local("global", b"c"));
		assert!(!is_node_local("global", b"clean_c2"));
		assert!(!is_node_local("pduid_pdu", b"clean_c"));
		assert!(!is_node_local("<column 7>", b"x"));
	}

	/// An operation `iterate_cf` does not report must not slip through as
	/// "nothing here".
	#[test]
	fn unseen_operation_type_is_unreadable() {
		let t = TestDb::new("range");
		let after = t.seq();
		let mut batch = WriteBatch::default();
		batch.delete_range_cf(&t.db().cf_handle("pduid_pdu").unwrap(), b"a", b"z");
		t.db().write(&batch).unwrap();
		assert!(t.seq() > after, "the range delete consumed a sequence");
		assert!(matches!(t.check(after, t.seq()), TailCheck::Unreadable(_)));
	}

	fn stop_record(dir: &Path, instance: &str, seq: u64) {
		write_synced(&dir.join(CLEAN_STOP_FILE), format!("{instance} {seq}\n").as_bytes()).unwrap();
	}

	fn scratch(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"tuwunel-identity-test-{tag}-{}-{}",
			std::process::id(),
			rand::string(8)
		));
		fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn clean_stop_keeps_identity_and_records_across_a_reboot() {
		let dir = scratch("clean");
		let (id, stop) = open_identity(&dir, 10, Some("boot-1")).unwrap();
		assert_eq!(stop, None, "first open has no previous run");
		mark_restored_checkpoint(&dir, "primary").unwrap();
		write_synced(&dir.join(HANDOFF_FILE), b"a 1 2 b\n").unwrap();

		stop_record(&dir, &id, 25);
		let (again, stop) = open_identity(&dir, 25, Some("boot-2")).unwrap();
		assert_eq!(again, id);
		assert_eq!(stop, Some(CleanStop { last_seq: 25 }));
		assert!(dir.join(PURE_FILE).exists());
		assert!(dir.join(HANDOFF_FILE).exists());
		let _ = fs::remove_dir_all(&dir);
	}

	/// A process crash: nothing a replica saw can have been lost, so the
	/// identity stands — but nothing vouches for the records any more.
	#[test]
	fn unclean_stop_on_the_same_boot_keeps_identity_but_drops_records() {
		let dir = scratch("crash");
		let (id, _) = open_identity(&dir, 10, Some("boot-1")).unwrap();
		mark_restored_checkpoint(&dir, "primary").unwrap();
		write_synced(&dir.join(HANDOFF_FILE), b"a 1 2 b\n").unwrap();

		let (again, stop) = open_identity(&dir, 40, Some("boot-1")).unwrap();
		assert_eq!(again, id);
		assert_eq!(stop, None);
		assert!(!dir.join(PURE_FILE).exists());
		assert!(!dir.join(HANDOFF_FILE).exists());
		let _ = fs::remove_dir_all(&dir);
	}

	/// A crash across a reboot may have lost the WAL tail: new identity.
	#[test]
	fn unclean_stop_across_a_reboot_replaces_identity() {
		let dir = scratch("power");
		let (id, _) = open_identity(&dir, 10, Some("boot-1")).unwrap();
		let (after_reboot, stop) = open_identity(&dir, 8, Some("boot-2")).unwrap();
		assert_ne!(after_reboot, id);
		assert_eq!(stop, None);

		// A stale clean-stop record (the database moved on past it) is the
		// same as none.
		stop_record(&dir, &after_reboot, 8);
		let (third, stop) = open_identity(&dir, 9, Some("boot-3")).unwrap();
		assert_ne!(third, after_reboot);
		assert_eq!(stop, None);
		let _ = fs::remove_dir_all(&dir);
	}

	/// Where the boot cannot be identified, an unclean stop is treated as a
	/// reboot.
	#[test]
	fn unclean_stop_with_unknown_boot_replaces_identity() {
		let dir = scratch("unknown");
		let (id, _) = open_identity(&dir, 10, None).unwrap();
		let (again, _) = open_identity(&dir, 10, None).unwrap();
		assert_ne!(again, id);
		let _ = fs::remove_dir_all(&dir);
	}

	/// A directory just restored from a checkpoint has no instance file. The
	/// marker the restore wrote must survive its first open.
	#[test]
	fn restored_checkpoint_keeps_its_marker_on_first_open() {
		let dir = scratch("restored");
		mark_restored_checkpoint(&dir, "primary").unwrap();
		let (_, stop) = open_identity(&dir, 500, Some("boot-1")).unwrap();
		assert_eq!(stop, None);
		assert_eq!(Purity::new(&dir).pure_of().as_deref(), Some("primary"));
		assert!(dir.join(SYNCED_FILE).exists());

		// The exact-copy marker goes at the first unclean stop; the record
		// that this database was synced does not — it is never re-copied for
		// that.
		let (_, _) = open_identity(&dir, 600, Some("boot-2")).unwrap();
		assert_eq!(Purity::new(&dir).pure_of(), None);
		assert!(dir.join(SYNCED_FILE).exists());
		let _ = fs::remove_dir_all(&dir);
	}
}
