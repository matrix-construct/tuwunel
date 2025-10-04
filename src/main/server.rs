use std::{path::PathBuf, sync::Arc};

use tokio::sync::Mutex;
use tuwunel_core::{
	Error, Result,
	config::Config,
	implement, info,
	log::Log,
	utils::{stream, sys},
};

use crate::{Args, args, logging::TracingFlameGuard, runtime};

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
pub fn new(args: Option<&Args>, runtime: Option<&runtime::Handle>) -> Result<Arc<Self>, Error> {
	let _runtime_guard = runtime.map(runtime::Handle::enter);

	let args_default = args.is_none().then(Args::default);
	let args = args.unwrap_or_else(|| args_default.as_ref().expect("default arguments"));
	let config_paths = args
		.config
		.as_deref()
		.into_iter()
		.flat_map(<[_]>::iter)
		.map(PathBuf::as_path);

	let config = Config::load(config_paths)
		.and_then(|raw| args::update(raw, args))
		.and_then(|raw| Config::new(&raw))?;

	let (tracing_reload_handle, tracing_flame_guard, capture) = crate::logging::init(&config)?;

	config.check()?;

	#[cfg(feature = "sentry_telemetry")]
	let sentry_guard = crate::sentry::init(&config);

	#[cfg(unix)]
	sys::maximize_fd_limit()
		.expect("Unable to increase maximum soft and hard file descriptor limit");

	let (_old_width, _new_width) = stream::set_width(config.stream_width_default);
	let (_old_amp, _new_amp) = stream::set_amplification(config.stream_amplification);

	info!(
		server_name = %config.server_name,
		database_path = ?config.database_path,
		log_levels = %config.log,
		"{}",
		tuwunel_core::version(),
	);

	let logger = Log { reload: tracing_reload_handle, capture };

	Ok(Arc::new(Self {
		server: Arc::new(tuwunel_core::Server::new(config, runtime.cloned(), logger)),

		services: None.into(),

		_tracing_flame_guard: tracing_flame_guard,

		#[cfg(feature = "sentry_telemetry")]
		_sentry_guard: sentry_guard,

		#[cfg(all(tuwunel_mods, feature = "tuwunel_mods"))]
		mods: tokio::sync::RwLock::new(Vec::new()),
	}))
}
