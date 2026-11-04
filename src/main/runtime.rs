#![cfg_attr(not(tokio_unstable), expect(unused))]

use std::{
	cell::OnceCell,
	io,
	iter::once,
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	thread,
	time::Duration,
};

use tokio::runtime::Builder;
#[cfg(tokio_unstable)]
use tokio::runtime::HistogramConfiguration;
pub use tokio::runtime::{Handle, Runtime as Tokio};
#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
use tuwunel_core::result::LogDebugErr;
use tuwunel_core::{
	Result, debug, error, implement, is_true,
	metrics::{Metrics, dump},
	utils::sys::{
		self,
		compute::{nth_core_available, set_affinity},
		max_threads,
	},
};

pub struct Runtime {
	runtime: OnceCell<Tokio>,
	metrics: Arc<Metrics>,
	state: Arc<State>,
}

#[derive(Default)]
struct State {
	metrics_dump: Option<PathBuf>,
	usage_dump: Option<PathBuf>,
	worker_affinity: Option<bool>,
	gc_on_park: Option<bool>,
	gc_muzzy: Option<bool>,
	cores_occupied: AtomicUsize,
	thread_spawns: AtomicUsize,
}

const WORKER_THREAD_NAME: &str = "tuwunel:worker";
const WORKER_THREAD_MIN: usize = 2;
const BLOCKING_THREAD_KEEPALIVE: u64 = 36;
const BLOCKING_THREAD_NAME: &str = "tuwunel:spawned";
const BLOCKING_THREAD_MAX: usize = 1024;
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const DISABLE_MUZZY_THRESHOLD: usize = 8;

#[implement(Runtime)]
pub fn new(args: Option<&crate::Args>) -> Result<Self> {
	let args_default = args.is_none().then(crate::Args::default);

	let args = args.unwrap_or_else(|| args_default.as_ref().expect("default arguments"));

	let max_blocking_threads = max_threads()
		.expect("obtained RLIMIT_NPROC or default")
		.0
		.saturating_div(3)
		.clamp(WORKER_THREAD_MIN, BLOCKING_THREAD_MAX);

	let state = Arc::new(State {
		metrics_dump: args.runtime_metrics_dir.clone(),
		usage_dump: args.runtime_usage_dir.clone(),
		worker_affinity: Some(args.worker_affinity),
		gc_on_park: args.gc_on_park,
		gc_muzzy: args.gc_muzzy,
		..Default::default()
	});

	let runtime = build_tokio(args, max_blocking_threads, &state)?;

	Ok(Self {
		metrics: Metrics::new(runtime.handle().into()),
		runtime: runtime.into(),
		state,
	})
}

fn build_tokio(
	args: &crate::Args,
	max_blocking_threads: usize,
	state: &Arc<State>,
) -> io::Result<Tokio> {
	let mut builder = Builder::new_multi_thread();
	builder
		.enable_io()
		.enable_time()
		.worker_threads(args.worker_threads.max(WORKER_THREAD_MIN))
		.max_blocking_threads(max_blocking_threads)
		.thread_keep_alive(Duration::from_secs(BLOCKING_THREAD_KEEPALIVE))
		.global_queue_interval(args.global_event_interval)
		.event_interval(args.kernel_event_interval)
		.max_io_events_per_tick(args.kernel_events_per_tick)
		.max_io_events_per_busy_tick(args.kernel_events_per_busy_tick);

	state.enable_hooks(&mut builder);

	#[cfg(tokio_unstable)]
	enable_poll_histogram(&mut builder, args);

	#[cfg(tokio_unstable)]
	enable_sched_histogram(&mut builder, args);

	builder.build()
}

impl Drop for Runtime {
	#[tracing::instrument(name = "stop", level = "info", skip_all)]
	fn drop(&mut self) {
		self.wait_shutdown();

		#[cfg(tokio_unstable)]
		self.dump_runtime_metrics();
		self.dump_resource_usage();
	}
}

#[cfg(tokio_unstable)]
#[implement(Runtime)]
fn dump_runtime_metrics(&self) {
	use tracing::Level;
	use tuwunel_core::event;

	// The final metrics output is promoted to INFO when tokio_unstable is active in
	// a release/bench mode and DEBUG is likely optimized out
	const IS_DEBUG: bool = cfg!(not(any(tokio_unstable, feature = "release_max_log_level")));

	const LEVEL: Level = if IS_DEBUG { Level::DEBUG } else { Level::INFO };

	let Some(runtime_metrics) = self.metrics.runtime_interval() else {
		return;
	};

	event!(LEVEL, ?runtime_metrics, "Final runtime metrics.");

	if let Some(dir) = self.state.metrics_dump.as_deref() {
		dump::write_runtime_metrics(dir, &runtime_metrics);
	}
}

#[implement(Runtime)]
fn dump_resource_usage(&self) {
	let Some(dir) = self.state.usage_dump.as_deref() else {
		return;
	};

	match sys::usage() {
		| Ok(usage) => dump::write_resource_usage(dir, &usage),
		| Err(error) => error!(
			%error,
			"Failed to read getrusage at exit."
		),
	}
}

#[implement(Runtime)]
fn wait_shutdown(&mut self) {
	debug!(
		timeout = ?RUNTIME_SHUTDOWN_TIMEOUT,
		"Waiting for runtime..."
	);

	if let Some(runtime) = self.runtime.take() {
		runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
	}

	// Join any jemalloc threads so they don't appear in use at exit.
	#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
	tuwunel_core::alloc::je::background_thread_enable(false)
		.log_debug_err()
		.ok();
}

#[implement(Runtime)]
#[inline]
pub fn block_on<F: Future>(&self, future: F) -> F::Output { self.runtime().block_on(future) }

#[implement(Runtime)]
#[inline]
pub fn metrics(&self) -> Arc<Metrics> { self.metrics.clone() }

#[implement(Runtime)]
#[inline]
pub fn handle(&self) -> &Handle { self.runtime().handle() }

#[implement(Runtime)]
#[inline]
pub fn runtime(&self) -> &Tokio {
	self.runtime
		.get()
		.expect("Runtime must be initialized")
}

#[cfg(tokio_unstable)]
fn enable_poll_histogram(builder: &mut Builder, args: &crate::Args) {
	let linear =
		linear_histogram(args.worker_poll_histogram_interval, args.worker_poll_histogram_buckets);

	builder
		.enable_metrics_poll_time_histogram()
		.metrics_poll_time_histogram_configuration(linear);
}

#[cfg(tokio_unstable)]
fn enable_sched_histogram(builder: &mut Builder, args: &crate::Args) {
	let linear = linear_histogram(
		args.worker_sched_histogram_interval,
		args.worker_sched_histogram_buckets,
	);

	builder
		.enable_metrics_schedule_latency_histogram()
		.metrics_schedule_latency_histogram_configuration(linear);
}

#[cfg(tokio_unstable)]
fn linear_histogram(micros: u64, buckets: usize) -> HistogramConfiguration {
	HistogramConfiguration::linear(Duration::from_micros(micros), buckets)
}

#[implement(State)]
fn enable_hooks(self: &Arc<Self>, builder: &mut Builder) {
	{
		let state = self.clone();
		builder.thread_name_fn(move || state.thread_name())
	};
	{
		let state = self.clone();
		builder.on_thread_start(move || state.thread_start())
	};
	{
		let state = self.clone();
		builder.on_thread_stop(move || state.thread_stop())
	};
	{
		let state = self.clone();
		builder.on_thread_unpark(move || state.thread_unpark())
	};
	{
		let state = self.clone();
		builder.on_thread_park(move || state.thread_park())
	};
	#[cfg(tokio_unstable)]
	{
		let state = self.clone();
		builder.on_task_spawn(move |meta| state.task_spawn(meta))
	};
	#[cfg(tokio_unstable)]
	{
		let state = self.clone();
		builder.on_before_task_poll(move |meta| state.task_enter(meta))
	};
	#[cfg(tokio_unstable)]
	{
		let state = self.clone();
		builder.on_after_task_poll(move |meta| state.task_leave(meta))
	};
	#[cfg(tokio_unstable)]
	{
		let state = self.clone();
		builder.on_task_terminate(move |meta| state.task_terminate(meta))
	};
}

#[implement(State)]
fn thread_name(&self) -> String {
	let handle = Handle::current();
	let num_workers = handle.metrics().num_workers();
	let i = self.thread_spawns.load(Ordering::Acquire);

	if i >= num_workers {
		BLOCKING_THREAD_NAME.into()
	} else {
		WORKER_THREAD_NAME.into()
	}
}

#[implement(State)]
#[tracing::instrument(
	name = "fork",
	level = "debug",
	skip_all,
	fields(
		tid = ?thread::current().id(),
		name = %thread::current().name().unwrap_or("None"),
	),
)]
fn thread_start(&self) {
	debug_assert!(
		thread::current().name() == Some(WORKER_THREAD_NAME)
			|| thread::current().name() == Some(BLOCKING_THREAD_NAME),
		"tokio worker name mismatch at thread start"
	);

	if self.worker_affinity.is_some_and(is_true!()) {
		self.set_worker_affinity();
	}

	self.thread_spawns.fetch_add(1, Ordering::AcqRel);
}

#[implement(State)]
fn set_worker_affinity(&self) {
	let handle = Handle::current();
	let num_workers = handle.metrics().num_workers();
	let i = self.cores_occupied.fetch_add(1, Ordering::AcqRel);
	if i >= num_workers {
		return;
	}

	let Some(id) = nth_core_available(i) else {
		return;
	};

	set_affinity(once(id));
	self.set_worker_mallctl(id);
}

#[implement(State)]
fn set_worker_mallctl(&self, _id: usize) {
	let muzzy_auto_disable =
		tuwunel_core::utils::available_parallelism() >= DISABLE_MUZZY_THRESHOLD;

	if matches!(self.gc_muzzy, Some(false) | None if muzzy_auto_disable) {
		#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
		tuwunel_core::alloc::je::this_thread::set_muzzy_decay(-1)
			.log_debug_err()
			.ok();
	}
}

#[implement(State)]
#[tracing::instrument(
	name = "join",
	level = "debug",
	skip_all,
	fields(
		tid = ?thread::current().id(),
		name = %thread::current().name().unwrap_or("None"),
	),
)]
#[expect(clippy::unused_self)]
fn thread_stop(&self) {
	if cfg!(any(tokio_unstable, not(feature = "release_max_log_level")))
		&& let Ok(resource_usage) = sys::thread_usage()
	{
		tuwunel_core::debug!(?resource_usage, "Thread resource usage.");
	}
}

#[implement(State)]
#[tracing::instrument(
	name = "work",
	level = "trace",
	skip_all,
	fields(
		tid = ?thread::current().id(),
		name = %thread::current().name().unwrap_or("None"),
	),
)]
#[expect(clippy::unused_self)]
fn thread_unpark(&self) {}

#[implement(State)]
#[tracing::instrument(
	name = "park",
	level = "trace",
	skip_all,
	fields(
		tid = ?thread::current().id(),
		name = %thread::current().name().unwrap_or("None"),
	),
)]
fn thread_park(&self) {
	match self.gc_on_park {
		| Some(true) | None if cfg!(feature = "jemalloc_conf") => gc_on_park(),
		| _ => (),
	}
}

fn gc_on_park() {
	#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
	tuwunel_core::alloc::je::this_thread::decay()
		.log_debug_err()
		.ok();
}

#[cfg(tokio_unstable)]
#[implement(State)]
#[tracing::instrument(
	name = "spawn",
	level = "trace",
	skip_all,
	fields(
		id = %meta.id(),
		tid = ?thread::current().id(),
	),
)]
#[expect(clippy::unused_self)]
fn task_spawn(&self, meta: &tokio::runtime::TaskMeta<'_>) {}

#[cfg(tokio_unstable)]
#[implement(State)]
#[tracing::instrument(
	name = "finish",
	level = "trace",
	skip_all,
	fields(
		id = %meta.id(),
		tid = ?thread::current().id()
	),
)]
#[expect(clippy::unused_self)]
fn task_terminate(&self, meta: &tokio::runtime::TaskMeta<'_>) {}

#[cfg(tokio_unstable)]
#[implement(State)]
#[tracing::instrument(
	name = "enter",
	level = "trace",
	skip_all,
	fields(
		id = %meta.id(),
		tid = ?thread::current().id()
	),
)]
#[expect(clippy::unused_self)]
fn task_enter(&self, meta: &tokio::runtime::TaskMeta<'_>) {}

#[cfg(tokio_unstable)]
#[implement(State)]
#[tracing::instrument(
	name = "leave",
	level = "trace",
	skip_all,
	fields(
		id = %meta.id(),
		tid = ?thread::current().id()
	),
)]
#[expect(clippy::unused_self)]
fn task_leave(&self, meta: &tokio::runtime::TaskMeta<'_>) {}

#[cfg(test)]
mod tests {
	use clap::Parser;

	use super::Runtime;
	use crate::Args;

	// The builder has no getter, so its refusal is the only proof the field arrived.
	#[test]
	#[should_panic(expected = "max_io_events_per_busy_tick must be non-zero")]
	fn busy_tick_reaches_the_builder() {
		let args = Args {
			kernel_events_per_busy_tick: 0,
			..Args::parse_from(["tuwunel"])
		};

		Runtime::new(Some(&args)).ok();
	}
}
