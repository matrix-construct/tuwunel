use rocksdb::{
	Env,
	event_listener::{
		CompactionJobInfo, DBBackgroundErrorReason, DBWriteStallCondition, EventListener,
		FlushJobInfo, IngestionInfo, MemTableInfo, MutableStatus, SubcompactionJobInfo,
		WriteStallInfo,
	},
};
use tuwunel_core::{Config, debug, debug::INFO_SPAN_LEVEL, debug_info, error, info, warn};

/// Bridges RocksDB background events into structured tracing.
///
/// The listener records stalls, compactions, flushes, memtable seals, and
/// external file ingestion without retaining per-event state.
pub(super) struct Events;

impl Events {
	pub(super) fn new(_config: &Config, _env: &Env) -> Self { Self {} }
}

impl EventListener for Events {
	#[tracing::instrument(name = "error", level = "error", skip_all)]
	fn on_background_error(&self, reason: DBBackgroundErrorReason, _status: MutableStatus) {
		error!(error = ?reason, "Critical RocksDB Error");
	}

	#[tracing::instrument(name = "stall", level = "warn", skip_all)]
	fn on_stall_conditions_changed(&self, info: &WriteStallInfo) {
		let col = info.cf_name();
		let col = col
			.as_deref()
			.map(str::from_utf8)
			.expect("column has a name")
			.expect("column name is valid utf8");

		let prev = info.prev();
		match info.cur() {
			| DBWriteStallCondition::KStopped => {
				error!(?col, ?prev, "Database Stalled");
			},
			| DBWriteStallCondition::KDelayed if prev == DBWriteStallCondition::KStopped => {
				warn!(?col, ?prev, "Database Stall Recovering");
			},
			| DBWriteStallCondition::KDelayed => {
				warn!(?col, ?prev, "Database Stalling");
			},
			| DBWriteStallCondition::KNormal
				if prev == DBWriteStallCondition::KStopped
					|| prev == DBWriteStallCondition::KDelayed =>
			{
				info!(?col, ?prev, "Database Stall Recovered");
			},
			| DBWriteStallCondition::KNormal => {
				debug!(?col, ?prev, "Database Normal");
			},
		}
	}

	#[tracing::instrument(
		name = "compaction",
		level = INFO_SPAN_LEVEL,
		skip_all,
	)]
	fn on_compaction_begin(&self, info: &CompactionJobInfo) {
		let col = info.cf_name();
		let col = col
			.as_deref()
			.map(str::from_utf8)
			.expect("column has a name")
			.expect("column name is valid utf8");

		let level = (info.base_input_level(), info.output_level());
		let records = (info.input_records(), info.output_records());
		let bytes = (info.total_input_bytes(), info.total_output_bytes());
		let files = (
			info.input_file_count(),
			info.output_file_count(),
			info.num_input_files_at_output_level(),
		);

		debug!(
			status = ?info.status(),
			?level,
			?files,
			?records,
			?bytes,
			micros = info.elapsed_micros(),
			errs = info.num_corrupt_keys(),
			reason = ?info.compaction_reason(),
			?col,
			"Compaction Starting",
		);
	}

	#[tracing::instrument(
		name = "compaction",
		level = INFO_SPAN_LEVEL,
		skip_all,
	)]
	fn on_compaction_completed(&self, info: &CompactionJobInfo) {
		let col = info.cf_name();
		let col = col
			.as_deref()
			.map(str::from_utf8)
			.expect("column has a name")
			.expect("column name is valid utf8");

		let level = (info.base_input_level(), info.output_level());
		let records = (info.input_records(), info.output_records());
		let bytes = (info.total_input_bytes(), info.total_output_bytes());
		let files = (
			info.input_file_count(),
			info.output_file_count(),
			info.num_input_files_at_output_level(),
		);

		debug_info!(
			status = ?info.status(),
			?level,
			?files,
			?records,
			?bytes,
			micros = info.elapsed_micros(),
			errs = info.num_corrupt_keys(),
			reason = ?info.compaction_reason(),
			?col,
			"Compaction Complete",
		);
	}

	#[tracing::instrument(name = "compaction", level = "debug", skip_all)]
	fn on_subcompaction_begin(&self, info: &SubcompactionJobInfo) {
		let col = info.cf_name();
		let col = col
			.as_deref()
			.map(str::from_utf8)
			.expect("column has a name")
			.expect("column name is valid utf8");

		let level = (info.base_input_level(), info.output_level());

		debug!(
			status = ?info.status(),
			?level,
			tid = info.thread_id(),
			reason = ?info.compaction_reason(),
			?col,
			"Compaction Starting",
		);
	}

	#[tracing::instrument(name = "compaction", level = "debug", skip_all)]
	fn on_subcompaction_completed(&self, info: &SubcompactionJobInfo) {
		let col = info.cf_name();
		let col = col
			.as_deref()
			.map(str::from_utf8)
			.expect("column has a name")
			.expect("column name is valid utf8");

		let level = (info.base_input_level(), info.output_level());

		debug!(
			status = ?info.status(),
			?level,
			tid = info.thread_id(),
			reason = ?info.compaction_reason(),
			?col,
			"Compaction Complete",
		);
	}

	#[tracing::instrument(
		name = "flush",
		level = INFO_SPAN_LEVEL,
		skip_all,
	)]
	fn on_flush_begin(&self, info: &FlushJobInfo) {
		let col = info.cf_name();
		let col = col
			.as_deref()
			.map(str::from_utf8)
			.expect("column has a name")
			.expect("column name is valid utf8");

		debug!(
			seq_start = info.smallest_seqno(),
			seq_end = info.largest_seqno(),
			slow = info.triggered_writes_slowdown(),
			stop = info.triggered_writes_stop(),
			reason = ?info.flush_reason(),
			?col,
			"Flush Starting",
		);
	}

	#[tracing::instrument(
		name = "flush",
		level = INFO_SPAN_LEVEL,
		skip_all,
	)]
	fn on_flush_completed(&self, info: &FlushJobInfo) {
		let col = info.cf_name();
		let col = col
			.as_deref()
			.map(str::from_utf8)
			.expect("column has a name")
			.expect("column name is valid utf8");

		debug_info!(
			seq_start = info.smallest_seqno(),
			seq_end = info.largest_seqno(),
			slow = info.triggered_writes_slowdown(),
			stop = info.triggered_writes_stop(),
			reason = ?info.flush_reason(),
			?col,
			"Flush Complete",
		);
	}

	#[tracing::instrument(
		name = "memtable",
		level = INFO_SPAN_LEVEL,
		skip_all,
	)]
	fn on_memtable_sealed(&self, info: &MemTableInfo) {
		let col = info.cf_name();
		let col = col
			.as_deref()
			.map(str::from_utf8)
			.expect("column has a name")
			.expect("column name is valid utf8");

		debug_info!(
			seq_first = info.first_seqno(),
			seq_early = info.earliest_seqno(),
			ents = info.num_entries(),
			dels = info.num_deletes(),
			?col,
			"Buffer Filled",
		);
	}

	fn on_external_file_ingested(&self, _info: &IngestionInfo) {
		unimplemented!();
	}
}
