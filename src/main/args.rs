//! Integration with `clap`

use std::path::PathBuf;

use clap::{ArgAction, Parser};
use tuwunel_core::{
	Err, Result,
	config::{Figment, FigmentValue},
	err, toml,
	utils::available_parallelism,
};

/// Commandline arguments
#[derive(Parser, Debug)]
#[clap(
	about,
	long_about = None,
	name = "tuwunel",
	version = tuwunel_core::version(),
)]
pub struct Args {
	#[arg(short, long)]
	/// Path to the config TOML file (optional)
	pub config: Option<Vec<PathBuf>>,

	/// Override a configuration variable using TOML 'key=value' syntax
	#[arg(long, short('O'))]
	pub option: Vec<String>,

	/// Run in a stricter read-only --maintenance mode.
	#[arg(long)]
	pub read_only: bool,

	/// Run in maintenance mode while refusing connections.
	#[arg(long)]
	pub maintenance: bool,

	#[cfg(feature = "console")]
	/// Activate admin command console automatically after startup.
	#[arg(long, num_args(0))]
	pub console: bool,

	/// Execute console command automatically after startup.
	#[arg(long)]
	pub execute: Vec<String>,

	/// Set functional testing modes if available. Ex '--test=smoke'. Empty
	/// values are permitted for compatibility with testing and benchmarking
	/// frameworks which may simply pass `--test` to the same execution.
	#[arg(
		long,
		hide(true),
		num_args = 0..=1,
		require_equals(false),
		default_missing_value = "",
	)]
	pub test: Vec<String>,

	/// Compatibility option for benchmark frameworks which pass `--bench` to
	/// the same execution and must be silently accepted without error.
	#[arg(
		long,
		hide(true),
		num_args = 0..=1,
		require_equals(false),
		default_missing_value = "",
	)]
	pub bench: Vec<String>,

	/// Override the tokio worker_thread count.
	#[arg(
		long,
		hide(true),
		env = "TOKIO_WORKER_THREADS",
		default_value = available_parallelism().to_string(),
	)]
	pub worker_threads: usize,

	/// Override the tokio global_queue_interval.
	#[arg(
		long,
		hide(true),
		env = "TOKIO_GLOBAL_QUEUE_INTERVAL",
		default_value = "192"
	)]
	pub global_event_interval: u32,

	/// Override the tokio event_interval.
	#[arg(
		long,
		hide(true),
		env = "TOKIO_EVENT_INTERVAL",
		default_value = "512"
	)]
	pub kernel_event_interval: u32,

	/// Override the tokio max_io_events_per_tick.
	#[arg(
		long,
		hide(true),
		env = "TOKIO_MAX_IO_EVENTS_PER_TICK",
		default_value = "512"
	)]
	pub kernel_events_per_tick: usize,

	/// Set the histogram bucket size, in microseconds (tokio_unstable). Default
	/// is 25 microseconds. If the values of the histogram don't approach zero
	/// with the exception of the last bucket, try increasing this value to e.g.
	/// 50 or 100. Inversely, decrease to 10 etc if the histogram lacks
	/// resolution.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_HISTOGRAM_INTERVAL",
		default_value = "25"
	)]
	pub worker_histogram_interval: u64,

	/// Set the histogram bucket count (tokio_unstable). Default is 20.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_HISTOGRAM_BUCKETS",
		default_value = "20"
	)]
	pub worker_histogram_buckets: usize,

	/// Toggles worker affinity feature.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_WORKER_AFFINITY",
		action = ArgAction::Set,
		num_args = 0..=1,
		require_equals(false),
		default_value = "true",
		default_missing_value = "true",
	)]
	pub worker_affinity: bool,

	/// Toggles feature to promote memory reclamation by the operating system
	/// when tokio worker runs out of work.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_GC_ON_PARK",
		action = ArgAction::Set,
		num_args = 0..=1,
		require_equals(false),
	)]
	pub gc_on_park: Option<bool>,

	/// Toggles muzzy decay for jemalloc arenas associated with a tokio
	/// worker (when worker-affinity is enabled). Setting to false releases
	/// memory to the operating system using MADV_FREE without MADV_DONTNEED.
	/// Setting to false increases performance by reducing pagefaults, but
	/// resident memory usage appears high until there is memory pressure. The
	/// default is true unless the system has four or more cores.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_GC_MUZZY",
		action = ArgAction::Set,
		num_args = 0..=1,
		require_equals(false),
	)]
	pub gc_muzzy: Option<bool>,
}

impl Args {
	#[must_use]
	pub fn default_test(name: &[&str]) -> Self {
		let mut args = Self::default();
		args.test
			.extend(name.iter().copied().map(ToOwned::to_owned));
		args.option
			.push("server_name=\"localhost\"".into());
		args
	}
}

impl Default for Args {
	fn default() -> Self { Self::parse() }
}

/// Parse commandline arguments into structured data
#[must_use]
pub fn parse() -> Args { Args::parse() }

/// Synthesize any command line options with configuration file options.
pub fn update(mut config: Figment, args: &Args) -> Result<Figment> {
	if args.read_only {
		config = config.join(("rocksdb_read_only", true));
	}

	if args.maintenance || args.read_only {
		config = config.join(("startup_netburst", false));
		config = config.join(("listening", false));
	}

	#[cfg(feature = "console")]
	// Indicate the admin console should be spawned automatically if the
	// configuration file hasn't already.
	if args.console {
		config = config.join(("admin_console_automatic", true));
	}

	// Execute commands after any commands listed in configuration file
	config = config.adjoin(("admin_execute", &args.execute));

	// Update config with names of any functional-tests
	config = config.adjoin(("test", &args.test));

	// All other individual overrides can go last in case we have options which
	// set multiple conf items at once and the user still needs granular overrides.
	for option in &args.option {
		let (path, val) = option
			.split_once('=')
			.ok_or_else(|| err!("Missing '=' in -O/--option: {option:?}"))?;

		if path.is_empty() {
			return Err!("Missing key= in -O/--option: {option:?}");
		}

		if val.is_empty() {
			return Err!("Missing =val in -O/--option: {option:?}");
		}

		// The value has to pass for what would appear as a line in the TOML file.
		let val = toml::from_str::<FigmentValue>(option)?;

		// Figment::merge() overrides existing
		config = config.merge((path, val.find(path)));
	}

	Ok(config)
}
