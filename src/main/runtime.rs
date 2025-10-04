use std::{
	iter::once,
	sync::{
		Arc, OnceLock,
		atomic::{AtomicUsize, Ordering},
	},
	thread,
	time::Duration,
};

use tokio::runtime::Builder;
pub use tokio::runtime::{Handle, Runtime};
#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
use tuwunel_core::result::LogDebugErr;
use tuwunel_core::{
	Result, debug, is_true,
	utils::sys::compute::{nth_core_available, set_affinity},
};

use crate::{Args, Server};

const WORKER_NAME: &str = "tuwunel:worker";
const WORKER_MIN: usize = 2;
const WORKER_KEEPALIVE: u64 = 36;
const MAX_BLOCKING_THREADS: usize = 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(10000);
#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
const DISABLE_MUZZY_THRESHOLD: usize = 4;

static WORKER_AFFINITY: OnceLock<bool> = OnceLock::new();
static GC_ON_PARK: OnceLock<Option<bool>> = OnceLock::new();
static GC_MUZZY: OnceLock<Option<bool>> = OnceLock::new();

pub fn new(args: Option<&Args>) -> Result<Runtime> {
	let args_default = args.is_none().then(Args::default);
	let args = args.unwrap_or_else(|| args_default.as_ref().expect("default arguments"));

	WORKER_AFFINITY
		.set(args.worker_affinity)
		.expect("set WORKER_AFFINITY from program argument");

	GC_ON_PARK
		.set(args.gc_on_park)
		.expect("set GC_ON_PARK from program argument");

	GC_MUZZY
		.set(args.gc_muzzy)
		.expect("set GC_MUZZY from program argument");

	let mut builder = Builder::new_multi_thread();
	builder
		.enable_io()
		.enable_time()
		.thread_name(WORKER_NAME)
		.worker_threads(args.worker_threads.max(WORKER_MIN))
		.max_blocking_threads(MAX_BLOCKING_THREADS)
		.thread_keep_alive(Duration::from_secs(WORKER_KEEPALIVE))
		.global_queue_interval(args.global_event_interval)
		.event_interval(args.kernel_event_interval)
		.max_io_events_per_tick(args.kernel_events_per_tick)
		.on_thread_start(thread_start)
		.on_thread_stop(thread_stop)
		.on_thread_unpark(thread_unpark)
		.on_thread_park(thread_park);

	#[cfg(tokio_unstable)]
	builder
		.on_task_spawn(task_spawn)
		.on_before_task_poll(task_enter)
		.on_after_task_poll(task_leave)
		.on_task_terminate(task_terminate);

	#[cfg(tokio_unstable)]
	enable_histogram(&mut builder, args);

	builder.build().map_err(Into::into)
}

#[cfg(tokio_unstable)]
fn enable_histogram(builder: &mut Builder, args: &Args) {
	use tokio::runtime::HistogramConfiguration;

	let buckets = args.worker_histogram_buckets;
	let interval = Duration::from_micros(args.worker_histogram_interval);
	let linear = HistogramConfiguration::linear(interval, buckets);
	builder
		.enable_metrics_poll_time_histogram()
		.metrics_poll_time_histogram_configuration(linear);
}

#[cfg(tokio_unstable)]
#[tracing::instrument(name = "stop", level = "info", skip_all)]
pub fn shutdown(server: &Arc<Server>, runtime: Runtime) -> Result {
	use tracing::Level;

	// The final metrics output is promoted to INFO when tokio_unstable is active in
	// a release/bench mode and DEBUG is likely optimized out
	const LEVEL: Level = if cfg!(debug_assertions) {
		Level::DEBUG
	} else {
		Level::INFO
	};

	wait_shutdown(server, runtime);
	let runtime_metrics = server
		.server
		.metrics
		.runtime_interval()
		.unwrap_or_default();

	tuwunel_core::event!(LEVEL, ?runtime_metrics, "Final runtime metrics");
	Ok(())
}

#[cfg(not(tokio_unstable))]
#[tracing::instrument(name = "stop", level = "info", skip_all)]
pub fn shutdown(server: &Arc<Server>, runtime: Runtime) -> Result {
	wait_shutdown(server, runtime);
	Ok(())
}

fn wait_shutdown(_server: &Arc<Server>, runtime: Runtime) {
	debug!(
		timeout = ?SHUTDOWN_TIMEOUT,
		"Waiting for runtime..."
	);

	runtime.shutdown_timeout(SHUTDOWN_TIMEOUT);

	// Join any jemalloc threads so they don't appear in use at exit.
	#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
	tuwunel_core::alloc::je::background_thread_enable(false)
		.log_debug_err()
		.ok();
}

#[tracing::instrument(
	name = "fork",
	level = "debug",
	skip_all,
	fields(
		id = ?thread::current().id(),
		name = %thread::current().name().unwrap_or("None"),
	),
)]
fn thread_start() {
	debug_assert_eq!(
		Some(WORKER_NAME),
		thread::current().name(),
		"tokio worker name mismatch at thread start"
	);

	if WORKER_AFFINITY.get().is_some_and(is_true!()) {
		set_worker_affinity();
	}
}

fn set_worker_affinity() {
	static CORES_OCCUPIED: AtomicUsize = AtomicUsize::new(0);

	let handle = Handle::current();
	let num_workers = handle.metrics().num_workers();
	let i = CORES_OCCUPIED.fetch_add(1, Ordering::Relaxed);
	if i >= num_workers {
		return;
	}

	let Some(id) = nth_core_available(i) else {
		return;
	};

	set_affinity(once(id));
	set_worker_mallctl(id);
}

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
fn set_worker_mallctl(id: usize) {
	use tuwunel_core::alloc::je::{
		is_affine_arena,
		this_thread::{set_arena, set_muzzy_decay},
	};

	if is_affine_arena() {
		set_arena(id).log_debug_err().ok();
	}

	let muzzy_option = GC_MUZZY
		.get()
		.expect("GC_MUZZY initialized by runtime::new()");

	let muzzy_auto_disable =
		tuwunel_core::utils::available_parallelism() >= DISABLE_MUZZY_THRESHOLD;

	if matches!(muzzy_option, Some(false) | None if muzzy_auto_disable) {
		set_muzzy_decay(-1).log_debug_err().ok();
	}
}

#[cfg(any(not(feature = "jemalloc"), target_env = "msvc"))]
fn set_worker_mallctl(_: usize) {}

#[tracing::instrument(
	name = "join",
	level = "debug",
	skip_all,
	fields(
		id = ?thread::current().id(),
		name = %thread::current().name().unwrap_or("None"),
	),
)]
fn thread_stop() {}

#[tracing::instrument(
	name = "work",
	level = "trace",
	skip_all,
	fields(
		id = ?thread::current().id(),
		name = %thread::current().name().unwrap_or("None"),
	),
)]
fn thread_unpark() {}

#[tracing::instrument(
	name = "park",
	level = "trace",
	skip_all,
	fields(
		id = ?thread::current().id(),
		name = %thread::current().name().unwrap_or("None"),
	),
)]
fn thread_park() {
	match GC_ON_PARK
		.get()
		.as_ref()
		.expect("GC_ON_PARK initialized by runtime::new()")
	{
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
#[tracing::instrument(
	name = "spawn",
	level = "trace",
	skip_all,
	fields(
		id = %meta.id(),
	),
)]
fn task_spawn(meta: &tokio::runtime::TaskMeta<'_>) {}

#[cfg(tokio_unstable)]
#[tracing::instrument(
	name = "finish",
	level = "trace",
	skip_all,
	fields(
		id = %meta.id()
	),
)]
fn task_terminate(meta: &tokio::runtime::TaskMeta<'_>) {}

#[cfg(tokio_unstable)]
#[tracing::instrument(
	name = "enter",
	level = "trace",
	skip_all,
	fields(
		id = %meta.id()
	),
)]
fn task_enter(meta: &tokio::runtime::TaskMeta<'_>) {}

#[cfg(tokio_unstable)]
#[tracing::instrument(
	name = "leave",
	level = "trace",
	skip_all,
	fields(
		id = %meta.id()
	),
)]
fn task_leave(meta: &tokio::runtime::TaskMeta<'_>) {}
