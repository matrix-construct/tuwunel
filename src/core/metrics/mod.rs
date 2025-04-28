use std::sync::atomic::AtomicU32;

use tokio::runtime;
use tokio_metrics::TaskMonitor;
#[cfg(tokio_unstable)]
use tokio_metrics::{RuntimeIntervals, RuntimeMonitor};

pub struct Metrics {
	_runtime: Option<runtime::Handle>,

	runtime_metrics: Option<runtime::RuntimeMetrics>,

	task_monitor: Option<TaskMonitor>,

	#[cfg(tokio_unstable)]
	_runtime_monitor: Option<RuntimeMonitor>,

	#[cfg(tokio_unstable)]
	runtime_intervals: std::sync::Mutex<Option<RuntimeIntervals>>,

	// TODO: move stats
	pub requests_handle_active: AtomicU32,
	pub requests_handle_finished: AtomicU32,
	pub requests_panic: AtomicU32,
}

impl Metrics {
	#[must_use]
	pub fn new(runtime: Option<runtime::Handle>) -> Self {
		#[cfg(tokio_unstable)]
		let runtime_monitor = runtime.as_ref().map(RuntimeMonitor::new);

		#[cfg(tokio_unstable)]
		let runtime_intervals = runtime_monitor
			.as_ref()
			.map(RuntimeMonitor::intervals);

		Self {
			_runtime: runtime.clone(),

			runtime_metrics: runtime.as_ref().map(runtime::Handle::metrics),

			task_monitor: runtime.map(|_| TaskMonitor::new()),

			#[cfg(tokio_unstable)]
			_runtime_monitor: runtime_monitor,

			#[cfg(tokio_unstable)]
			runtime_intervals: std::sync::Mutex::new(runtime_intervals),

			requests_handle_active: AtomicU32::new(0),
			requests_handle_finished: AtomicU32::new(0),
			requests_panic: AtomicU32::new(0),
		}
	}

	#[cfg(tokio_unstable)]
	pub fn runtime_interval(&self) -> Option<tokio_metrics::RuntimeMetrics> {
		self.runtime_intervals
			.lock()
			.expect("locked")
			.as_mut()
			.map(Iterator::next)
			.expect("next interval")
	}

	#[inline]
	pub fn task_root(&self) -> Option<&TaskMonitor> {
		self.task_monitor.as_ref()
	}

	#[inline]
	pub fn num_workers(&self) -> usize {
		self.runtime_metrics()
			.map_or(0, runtime::RuntimeMetrics::num_workers)
	}

	#[inline]
	pub fn runtime_metrics(&self) -> Option<&runtime::RuntimeMetrics> {
		self.runtime_metrics.as_ref()
	}
}
