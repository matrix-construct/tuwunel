//! Integration with `clap`

use std::path::PathBuf;

use clap::{ArgAction, Parser};
use tuwunel_core::{
	Err, Result,
	config::{Figment, FigmentValue},
	err, implement, is_true, toml,
	utils::available_parallelism,
};

/// Only its own argument may set this, since restoring is destructive and
/// one-shot for the invocation which asked for it. `Err!(Config(..))` takes a
/// literal, so the two refusal sites spell the key out again; renaming here is
/// not a single-site edit.
const RESTORE_KEY: &str = "database_restore_backup";

const RESTORE_REFUSAL: &str =
	"Only the --restore-backup command line argument may set this option.";

/// Commandline arguments
#[derive(Clone, Parser, Debug)]
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

	/// Probe a running server for liveness and exit; the running server must
	/// share this configuration.
	#[arg(long, conflicts_with = "config_command")]
	pub health_check: bool,

	/// Write a pristine example configuration and exit.
	///
	/// With no path, the document is written to standard output.
	#[arg(
		long,
		num_args = 0..=1,
		require_equals = true,
		group = "config_command",
	)]
	pub generate_config: Option<Option<PathBuf>>,

	/// Rewrite the configured files in the current example-file shape and exit.
	///
	/// With no path, a single input writes beside itself with a `.new` suffix.
	/// Multiple inputs require an explicit destination because their layers are
	/// collapsed into one document.
	#[arg(
		long,
		num_args = 0..=1,
		require_equals = true,
		group = "config_command",
	)]
	pub regenerate_config: Option<Option<PathBuf>>,

	/// Replace an existing generated configuration file.
	///
	/// The previous contents are retained in a backup file.
	#[arg(long, requires = "config_command")]
	pub force: bool,

	/// Include configuration values supplied through the environment.
	///
	/// By default, regeneration retains only values supplied by files.
	#[arg(long, requires = "regenerate_config")]
	pub include_env: bool,

	/// Comment out deprecated and unknown keys in regenerated output.
	///
	/// By default, these values stay active to preserve existing behavior.
	#[arg(long, requires = "regenerate_config")]
	pub strip_unknown: bool,

	/// Restore an online database backup on startup, before the database is
	/// opened, then continue starting up normally. The optional value is a
	/// backup ID as listed by '!admin server list-backups'; the most recent
	/// backup is restored when no ID is given.
	#[arg(
		long,
		num_args = 0..=1,
		require_equals(false),
		default_missing_value = "0",
		conflicts_with = "config_command",
	)]
	pub restore_backup: Option<u32>,

	#[cfg(feature = "console")]
	/// Activate admin command console automatically after startup. Activation
	/// requires standard input to be a terminal.
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

	/// Set the poll histogram bucket size, in microseconds (tokio_unstable).
	///
	/// Default is 20 microseconds. If the values of the histogram don't
	/// approach zero with the exception of the last bucket, try increasing this
	/// value to e.g. 50 or 100. Inversely, decrease to 10 etc if the histogram
	/// lacks resolution.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_POLL_HISTOGRAM_INTERVAL",
		default_value = "20"
	)]
	pub worker_poll_histogram_interval: u64,

	/// Set the poll histogram bucket count (tokio_unstable).
	///
	/// Default is 15.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_POLL_HISTOGRAM_BUCKETS",
		default_value = "15"
	)]
	pub worker_poll_histogram_buckets: usize,

	/// Set the scheduler histogram bucket size, in microseconds
	/// (tokio_unstable).
	///
	/// Default is 10 microseconds. This histogram measures the delay between a
	/// task being scheduled and a worker polling it, so it is tuned against
	/// queueing delay rather than the poll duration measured by the poll
	/// histogram. Increase this value to e.g. 50 or 100 when only the last
	/// bucket is populated; decrease it to 10 etc when everything lands in the
	/// first bucket.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_SCHED_HISTOGRAM_INTERVAL",
		default_value = "10"
	)]
	pub worker_sched_histogram_interval: u64,

	/// Set the scheduler histogram bucket count (tokio_unstable).
	///
	/// Default is 15. Every bucket but the last spans one bucket size; the
	/// last is unbounded above, so the count and the bucket size together set
	/// the latency beyond which the histogram stops resolving.
	#[arg(
		long,
		hide(true),
		env = "TUWUNEL_RUNTIME_SCHED_HISTOGRAM_BUCKETS",
		default_value = "15"
	)]
	pub worker_sched_histogram_buckets: usize,

	/// Write tokio runtime metrics at exit to a file in the directory
	/// provided. The format will be JSON. The file will be named
	/// `tuwunel.runtime_metrics.<pid>.json`. The metrics are accumulated for
	/// the last runtime interval; total value is only obtained if this is the
	/// first call for the execution.
	#[arg(
		long,
		hide(true),
		num_args = 0..=1,
		require_equals(false),
		env = "TUWUNEL_RUNTIME_METRICS_DIR",
		default_missing_value = ""
	)]
	pub runtime_metrics_dir: Option<PathBuf>,

	/// Write system resource usage (`getrusage(2)`) metrics at exit to a file
	/// in the directory provided. The format will be JSON. The file will be
	/// named `tuwunel.runtime_usage.<pid>.json`.
	#[arg(
		long,
		hide(true),
		num_args = 0..=1,
		require_equals(false),
		env = "TUWUNEL_RUNTIME_USAGE_DIR",
		default_missing_value = ""
	)]
	pub runtime_usage_dir: Option<PathBuf>,

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
	/// default is true unless the system has eight or more cores.
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

/// Returns arguments for a test, naming the harnesses it opts into.
///
/// The server name is preset to `localhost`. A test wanting another appends its
/// own override, which wins, since the later value of a key takes precedence.
#[implement(Args)]
#[must_use]
pub fn default_test(name: &[&str]) -> Self {
	Self::default()
		.with_tests(name)
		.with_option("server_name=\"localhost\"")
}

/// Returns these arguments with more test harnesses appended.
///
/// Appends rather than replaces, because the arguments a process was launched
/// with may already name one.
#[implement(Args)]
#[must_use]
fn with_tests(mut self, names: &[&str]) -> Self {
	self.test
		.extend(names.iter().copied().map(ToOwned::to_owned));

	self
}

/// Returns these arguments with one more configuration override appended.
///
/// The override takes the TOML `key=value` syntax `-O` takes on the command
/// line.
#[implement(Args)]
#[must_use]
pub fn with_option<S: Into<String>>(mut self, option: S) -> Self {
	self.option.push(option.into());

	self
}

impl Default for Args {
	fn default() -> Self { Self::parse() }
}

/// Parse commandline arguments into structured data
#[must_use]
pub fn parse() -> Args { Args::parse() }

/// Synthesize any command line options with configuration file options.
pub fn update(mut config: Figment, args: &Args) -> Result<Figment> {
	if config
		.find_value("maintenance")
		.ok()
		.as_ref()
		.and_then(FigmentValue::to_bool)
		.is_some_and(is_true!())
	{
		return Err!(Config("maintenance", "Not permitted to set this option."));
	}

	if config.find_value(RESTORE_KEY).is_ok() {
		return Err!(Config("database_restore_backup", "{RESTORE_REFUSAL}"));
	}

	if let Some(backup_id) = args.restore_backup {
		config = config.join((RESTORE_KEY, backup_id));
	}

	if args.read_only {
		config = config.join(("rocksdb_read_only", true));
	}

	if args.maintenance || args.read_only {
		config = config.join(("maintenance", true));
		config = config.join(("listening", false));
		config = config.join(("startup_netburst", false));
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

		// The merge keys on this path, so an exact match is the whole surface.
		if path == RESTORE_KEY {
			return Err!(Config("database_restore_backup", "{RESTORE_REFUSAL}"));
		}

		// The value has to pass for what would appear as a line in the TOML file.
		let val = toml::from_str::<FigmentValue>(option)?;

		// Figment::merge() overrides existing
		config = config.merge((path, val.find(path)));
	}

	Ok(config)
}

#[cfg(test)]
mod tests {
	use std::ffi::OsString;

	use super::{Args, Figment, Parser, RESTORE_KEY, Result, update};

	#[test]
	fn the_restore_argument_sets_the_key() {
		let raw = updated(&["tuwunel", "--restore-backup", "5"], Figment::new())
			.expect("the argument is accepted");

		raw.find_value(RESTORE_KEY)
			.expect("the argument sets it");
	}

	fn updated(argv: &[&str], raw: Figment) -> Result<Figment> {
		update(raw, &Args::parse_from(argv))
	}

	#[test]
	fn an_option_may_not_set_the_restore_key() {
		let argv = ["tuwunel", "-O", "database_restore_backup=5"];
		let refusal = refusal(&argv, Figment::new());

		assert!(refusal.contains(RESTORE_KEY));
		assert!(refusal.contains("--restore-backup"));
	}

	fn refusal(argv: &[&str], raw: Figment) -> String {
		updated(argv, raw)
			.map(drop)
			.expect_err("refused")
			.to_string()
	}

	/// The exact comparison in the guard rests on figment matching keys
	/// exactly. These spellings never reach the option; were that to change,
	/// the guard would need widening and this is what would say so.
	#[test]
	fn indirect_spellings_do_not_reach_the_restore_key() {
		for option in [" database_restore_backup =5", r#""database_restore_backup"=5"#] {
			let raw = updated(&["tuwunel", "-O", option], Figment::new()).expect("accepted");

			raw.find_value(RESTORE_KEY)
				.expect_err("figment matches keys exactly");
		}
	}

	#[test]
	fn a_configured_restore_key_is_refused() {
		let raw = Figment::new().merge((RESTORE_KEY, 5));

		assert!(refusal(&["tuwunel"], raw).contains(RESTORE_KEY));
	}

	#[test]
	fn other_options_are_unaffected() {
		let argv = ["tuwunel", "-O", r#"server_name="pinned.example""#];
		let raw = updated(&argv, Figment::new()).expect("accepted");

		assert_eq!(
			raw.find_value("server_name")
				.expect("present")
				.into_string()
				.as_deref(),
			Some("pinned.example"),
		);
	}

	#[test]
	fn config_commands_accept_an_optional_equals_path() {
		let generate = Args::parse_from(["tuwunel".into(), long("generate-config")]);
		let generate_to =
			Args::parse_from(["tuwunel".into(), long("generate-config=fresh.toml")]);

		let regenerate = Args::parse_from(["tuwunel".into(), long("regenerate-config")]);
		let regenerate_to =
			Args::parse_from(["tuwunel".into(), long("regenerate-config=renewed.toml")]);

		assert_eq!(generate.generate_config, Some(None));
		assert_eq!(generate_to.generate_config, Some(Some("fresh.toml".into())));
		assert_eq!(regenerate.regenerate_config, Some(None));
		assert_eq!(regenerate_to.regenerate_config, Some(Some("renewed.toml".into())));
	}

	fn long(name: &str) -> OsString {
		let mut argument = OsString::with_capacity(name.len().saturating_add(2));

		argument.push("-");
		argument.push("-");
		argument.push(name);

		argument
	}

	#[test]
	fn config_command_paths_require_equals() {
		Args::try_parse_from(["tuwunel".into(), long("generate-config"), "fresh.toml".into()])
			.expect_err("generation path without equals sign rejected");

		Args::try_parse_from([
			"tuwunel".into(),
			long("regenerate-config"),
			"renewed.toml".into(),
		])
		.expect_err("regeneration path without equals sign rejected");
	}

	#[test]
	fn config_commands_are_mutually_exclusive() {
		Args::try_parse_from([
			"tuwunel".into(),
			long("generate-config"),
			long("regenerate-config"),
		])
		.expect_err("config commands are mutually exclusive");
	}

	#[test]
	fn config_commands_conflict_with_health_check() {
		for command in ["generate-config", "regenerate-config"] {
			Args::try_parse_from(["tuwunel".into(), long(command), long("health-check")])
				.expect_err("config command and health check are mutually exclusive");
		}
	}

	#[test]
	fn config_commands_conflict_with_restore_backup() {
		for command in ["generate-config", "regenerate-config"] {
			Args::try_parse_from(["tuwunel".into(), long(command), long("restore-backup")])
				.expect_err("config command and backup restore are mutually exclusive");
		}
	}

	#[test]
	fn config_command_controls_parse() {
		let generate = Args::parse_from([
			"tuwunel".into(),
			long("generate-config=fresh.toml"),
			long("force"),
		]);

		let regenerate = Args::parse_from([
			"tuwunel".into(),
			long("regenerate-config"),
			long("force"),
			long("include-env"),
			long("strip-unknown"),
		]);

		assert!(generate.force);
		assert!(regenerate.force);
		assert!(regenerate.include_env);
		assert!(regenerate.strip_unknown);
	}

	#[test]
	fn force_requires_a_config_command() {
		Args::try_parse_from(["tuwunel".into(), long("force")])
			.expect_err("force without a config command rejected");
	}

	#[test]
	fn regeneration_controls_require_the_command() {
		Args::try_parse_from(["tuwunel".into(), long("include-env")])
			.expect_err("environment inclusion without regeneration rejected");

		Args::try_parse_from(["tuwunel".into(), long("strip-unknown")])
			.expect_err("residue stripping without regeneration rejected");
	}
}
