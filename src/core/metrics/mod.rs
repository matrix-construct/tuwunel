pub mod dump;

use std::sync::{
	Arc, Mutex,
	atomic::{AtomicU32, AtomicU64},
};
#[cfg(tokio_unstable)]
use std::{iter::repeat, ops::Range, time::Duration};

#[cfg(tokio_unstable)]
use smallvec::SmallVec;
use tokio::runtime;
#[cfg(tokio_unstable)]
use tokio_metrics::{RuntimeIntervals, RuntimeMonitor};
use tokio_metrics::{TaskMetrics, TaskMonitor};

/// Bucket counts sampled from the scheduler latency histogram.
///
/// The inline budget matches the default bucket count; a runtime configured
/// with more buckets spills to the heap.
#[cfg(tokio_unstable)]
type Counts = SmallVec<[u64; 20]>;

/// One bucket of the scheduler latency histogram.
///
/// The range is the bucket's latency span as configured on the runtime; the
/// count is how many task schedules landed within it.
#[cfg(tokio_unstable)]
type Bucket = (Range<Duration>, u64);

/// Owns process-wide runtime, task, and request measurements.
///
/// Task monitoring is available in builds with Tokio's unstable metrics.
/// Runtime monitoring additionally requires an embedding runtime handle, while
/// request counters remain independent of either monitor.
pub struct Metrics {
	_runtime: Option<runtime::Handle>,

	runtime_metrics: Option<runtime::RuntimeMetrics>,

	task_monitor: Option<TaskMonitor>,

	task_intervals: Mutex<Option<Box<dyn Iterator<Item = TaskMetrics> + Send>>>,

	#[cfg(tokio_unstable)]
	_runtime_monitor: Option<RuntimeMonitor>,

	#[cfg(tokio_unstable)]
	runtime_intervals: Mutex<Option<RuntimeIntervals>>,

	#[cfg(tokio_unstable)]
	sched_histogram_last: Mutex<Counts>,

	// TODO: move stats
	/// Supplies identifiers for traced requests.
	///
	/// The tracing span consumes and increments the sequence when its fields
	/// are evaluated. It is not an unconditional request-traffic counter.
	pub requests_count: AtomicU64,

	/// Counts request handlers that have finished in debug builds.
	///
	/// The counter is monotonic and remains separate from the gauge of handlers
	/// still active. Release builds leave it at zero.
	pub requests_handle_finished: AtomicU64,

	/// Tracks request handlers currently executing in debug builds.
	///
	/// The value is a gauge rather than a lifetime total. Release builds leave
	/// it at zero.
	pub requests_handle_active: AtomicU32,

	/// Counts request handlers that terminated through panic handling.
	///
	/// The counter is scoped to request handling rather than all process
	/// panics. It is monotonic for the lifetime of the process.
	pub requests_panic: AtomicU32,
}

impl Metrics {
	#[must_use]
	/// Creates shared metrics state for an optional runtime.
	///
	/// A runtime handle enables runtime snapshots. Builds exposing Tokio's
	/// unstable metrics also install task monitoring independently of that
	/// handle.
	pub fn new(runtime: Option<&runtime::Handle>) -> Arc<Self> {
		#[cfg(tokio_unstable)]
		let runtime_monitor = runtime.map(RuntimeMonitor::new);

		#[cfg(tokio_unstable)]
		let runtime_intervals = runtime_monitor
			.as_ref()
			.map(RuntimeMonitor::intervals);

		let task_monitor = cfg!(tokio_unstable).then(|| {
			TaskMonitor::builder()
				.with_slow_poll_threshold(TaskMonitor::DEFAULT_SLOW_POLL_THRESHOLD)
				.with_long_delay_threshold(TaskMonitor::DEFAULT_LONG_DELAY_THRESHOLD)
				.clone()
				.build()
		});

		let task_intervals = task_monitor.as_ref().map(
			|task_monitor| -> Box<dyn Iterator<Item = TaskMetrics> + Send> {
				Box::new(task_monitor.intervals())
			},
		);

		Arc::new(Self {
			_runtime: runtime.cloned(),

			runtime_metrics: runtime.map(runtime::Handle::metrics),

			task_monitor,

			task_intervals: task_intervals.into(),

			#[cfg(tokio_unstable)]
			_runtime_monitor: runtime_monitor,

			#[cfg(tokio_unstable)]
			runtime_intervals: Mutex::new(runtime_intervals),

			#[cfg(tokio_unstable)]
			sched_histogram_last: Counts::new().into(),

			requests_count: AtomicU64::new(0),
			requests_handle_finished: AtomicU64::new(0),
			requests_handle_active: AtomicU32::new(0),
			requests_panic: AtomicU32::new(0),
		})
	}

	#[inline]
	/// Awaits a future under task instrumentation when a monitor is available.
	///
	/// Instrumented futures contribute to task interval snapshots. Builds
	/// without a task monitor await the future directly with no wrapper.
	pub async fn instrument<F, Output>(&self, f: F) -> Output
	where
		F: Future<Output = Output>,
	{
		if let Some(monitor) = self.task_metrics() {
			monitor.instrument(f).await
		} else {
			f.await
		}
	}

	/// Advances the task monitor and returns its next interval sample.
	///
	/// A server without task monitoring returns `None`. Samples describe only
	/// futures explicitly passed through [`Self::instrument`].
	///
	/// # Panics
	///
	/// Panics when the interval mutex is poisoned.
	pub fn task_interval(&self) -> Option<TaskMetrics> {
		self.task_intervals
			.lock()
			.expect("locked")
			.as_mut()
			.and_then(Iterator::next)
	}

	#[cfg(tokio_unstable)]
	/// Advances the runtime monitor and returns its next interval sample.
	///
	/// The returned value is absent only if the monitor's interval iterator
	/// ends. Calls require a runtime handle to have been supplied at
	/// construction.
	///
	/// # Panics
	///
	/// Panics when the interval mutex is poisoned or no runtime monitor exists.
	pub fn runtime_interval(&self) -> Option<tokio_metrics::RuntimeMetrics> {
		self.runtime_intervals
			.lock()
			.expect("locked")
			.as_mut()
			.map(Iterator::next)
			.expect("next interval")
	}

	/// Bucket deltas of the scheduler latency histogram since the last call.
	///
	/// Each call diffs the runtime's cumulative per-worker counts against the
	/// previous sample and replaces it, so the counts are the traffic of one
	/// interval rather than the totals tokio exposes. Returns `None` when the
	/// runtime was built without the histogram.
	///
	/// # Panics
	///
	/// Panics when the scheduler histogram state mutex is poisoned.
	#[cfg(tokio_unstable)]
	pub fn sched_histogram_interval(&self) -> Option<impl Iterator<Item = Bucket>> {
		let metrics = self
			.runtime_metrics()
			.filter(|metrics| metrics.schedule_latency_histogram_enabled())?;

		let num_workers = metrics.num_workers();
		let bucket_total = |bucket| -> u64 {
			(0..num_workers)
				.map(|worker| metrics.schedule_latency_histogram_bucket_count(worker, bucket))
				.sum()
		};

		let num_buckets = metrics.schedule_latency_histogram_num_buckets();
		let totals: Counts = (0..num_buckets).map(bucket_total).collect();

		let mut last = self.sched_histogram_last.lock().expect("locked");
		let deltas: Counts = totals
			.iter()
			.zip(last.iter().copied().chain(repeat(0)))
			.map(|(total, last)| total.saturating_sub(last))
			.collect();

		*last = totals;

		let ranges = (0..num_buckets)
			.map(move |bucket| metrics.schedule_latency_histogram_bucket_range(bucket));

		Some(ranges.zip(deltas))
	}

	#[inline]
	/// Returns the number of workers in the monitored runtime.
	///
	/// A server constructed without a runtime handle reports zero. The value
	/// comes directly from Tokio's runtime metrics snapshot.
	pub fn num_workers(&self) -> usize {
		self.runtime_metrics()
			.map_or(0, runtime::RuntimeMetrics::num_workers)
	}

	#[inline]
	/// Returns the optional task monitor.
	///
	/// The monitor exists only when task metrics were enabled at construction.
	/// It may be used to instrument futures or inspect cumulative task
	/// measurements.
	pub fn task_metrics(&self) -> Option<&TaskMonitor> { self.task_monitor.as_ref() }

	#[inline]
	/// Returns the optional Tokio runtime metrics handle.
	///
	/// The handle exists when server construction received an embedding
	/// runtime. Callers borrow it without advancing any interval monitor.
	pub fn runtime_metrics(&self) -> Option<&runtime::RuntimeMetrics> {
		self.runtime_metrics.as_ref()
	}
}
