use std::{iter::empty, sync::Arc};

use tokio::sync::Mutex;
use tuwunel_core::{
	Error, Result, Server as CoreServer,
	config::{Config, Figment, Sources},
	implement, info,
	metrics::Metrics,
	utils::{
		stream::{set_amplification, set_width},
		sys,
	},
};

use crate::{
	Args, Runtime, args::update as update_config, logging::TracingFlameGuard, runtime::Handle,
};

/// Server runtime state; complete
pub struct Server {
	/// Server runtime state; public portion
	pub server: Arc<tuwunel_core::Server>,

	pub services: Mutex<Option<Arc<tuwunel_service::Services>>>,

	_tracing_flame_guard: TracingFlameGuard,

	#[cfg(feature = "sentry_telemetry")]
	_sentry_guard: Option<::sentry::ClientInitGuard>,

	#[cfg(all(tuwunel_mods, feature = "tuwunel_mods"))]
	// Module instances; TODO: move to mods::loaded mgmt vector
	pub(crate) mods: tokio::sync::RwLock<Vec<tuwunel_core::mods::Module>>,
}

#[implement(Server)]
pub fn new(args: Option<&Args>, runtime: Option<&Runtime>) -> Result<Arc<Self>, Error> {
	let args_default = args.is_none().then(Args::default);

	let args = args.unwrap_or_else(|| args_default.as_ref().expect("default arguments"));

	let handle = runtime.map(Runtime::handle);

	let _runtime_guard = handle.map(Handle::enter);

	let metrics = runtime
		.map(Runtime::metrics)
		.unwrap_or_else(|| Metrics::new(None));

	let config_sources = config_sources(args);

	let config = config_sources
		.load(empty())
		.map(|raw| with_restore(raw, args))
		.and_then(|raw| Config::new(&raw))?;

	let (tracing_flame_guard, logger) = crate::logging::init(&config)?;

	config.check()?;

	#[cfg(feature = "sentry_telemetry")]
	let sentry_guard = crate::sentry::init(&config);

	sys::maximize_fd_limit().expect("Unable to increase maximum file descriptor limit");

	sys::maximize_thread_limit().expect("Unable to increase maximum thread count limit");

	let (_old_width, _new_width) = set_width(config.runtime.stream.width_default);
	let (_old_amp, _new_amp) = set_amplification(config.runtime.stream.amplification);

	info!(
		server_name = %config.server_name,
		database_path = ?config.database_path,
		log_levels = %config.logging.filter,
		"{}",
		tuwunel_core::version(),
	);

	Ok(Arc::new(Self {
		server: Arc::new(CoreServer::new(config, config_sources, handle, logger, metrics)),

		services: None.into(),

		_tracing_flame_guard: tracing_flame_guard,

		#[cfg(feature = "sentry_telemetry")]
		_sentry_guard: sentry_guard,

		#[cfg(all(tuwunel_mods, feature = "tuwunel_mods"))]
		mods: tokio::sync::RwLock::new(Vec::new()),
	}))
}

pub(crate) fn config_sources(args: &Args) -> Sources {
	// Taken out of the retained copy rather than cloned beside it, so the paths
	// have one home.
	let mut cli = args.clone();
	let paths = cli.config.take().unwrap_or_default();

	cli.restore_backup = None;

	Sources {
		paths,
		overrides: Some(Box::new(move |raw| update_config(raw, &cli))),
	}
}

/// Applied outside the retained sources, and so not replayed on reload, since
/// restoring is one-shot for the invocation which asked for it.
fn with_restore(raw: Figment, args: &Args) -> Figment {
	match args.restore_backup {
		| None => raw,
		| Some(backup_id) => raw.join(("database_restore_backup", backup_id)),
	}
}

#[cfg(test)]
mod tests {
	use std::path::PathBuf;

	use clap::Parser;
	use tuwunel_core::config::Figment;

	use super::{Args, config_sources, with_restore};

	fn args(argv: &[&str]) -> Args { Args::parse_from(argv) }

	#[test]
	fn startup_carries_the_restore_flag() {
		let raw = with_restore(Figment::new(), &args(&["tuwunel", "--restore-backup", "5"]));

		raw.find_value("database_restore_backup")
			.expect("the startup configuration carries it");
	}

	#[test]
	fn a_reload_does_not_replay_the_restore_flag() {
		let sources = config_sources(&args(&["tuwunel", "--restore-backup", "5"]));
		let overrides = sources.overrides.as_ref().expect("installed");

		overrides(Figment::new())
			.expect("applies")
			.find_value("database_restore_backup")
			.expect_err("a reload does not restore again");
	}

	#[test]
	fn a_reload_replays_the_other_overrides() {
		let sources =
			config_sources(&args(&["tuwunel", "-O", r#"server_name="pinned.example""#]));
		let overrides = sources.overrides.as_ref().expect("installed");

		let replayed = overrides(Figment::new()).expect("applies");

		assert_eq!(
			replayed
				.find_value("server_name")
				.expect("present")
				.into_string()
				.as_deref(),
			Some("pinned.example"),
		);
	}

	#[test]
	fn config_paths_are_retained() {
		let sources = config_sources(&args(&["tuwunel", "-c", "/etc/tuwunel/tuwunel.toml"]));

		assert_eq!(sources.paths, [PathBuf::from("/etc/tuwunel/tuwunel.toml")]);
	}
}
