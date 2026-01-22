use std::sync::Arc;

use tracing::subscriber::NoSubscriber;
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt, layer::SubscriberExt, reload};
use tuwunel_core::{
	Result,
	config::Config,
	debug_warn, err,
	log::{ConsoleFormat, ConsoleWriter, LogLevelReloadHandles, Logging, capture, fmt_span},
	result::UnwrapOrErr,
};

#[cfg(feature = "perf_measurements")]
pub(crate) type TracingFlameGuard =
	Option<tracing_flame::FlushGuard<std::io::BufWriter<std::fs::File>>>;

#[cfg(not(feature = "perf_measurements"))]
pub(crate) type TracingFlameGuard = Option<()>;

pub(crate) fn init(config: &Config) -> Result<(TracingFlameGuard, Logging)> {
	let reload_handles = LogLevelReloadHandles::default();
	let cap_state = Arc::new(capture::State::new());

	if !config.log_enable {
		return Ok((None, Logging {
			reload: reload_handles,
			capture: cap_state,
			subscriber: Arc::new(NoSubscriber::new()),
		}));
	}

	let console_span_events = fmt_span::from_str(&config.log_span_events).unwrap_or_err();

	let console_filter = EnvFilter::builder()
		.with_regex(config.log_filter_regex)
		.parse(&config.log)
		.map_err(|e| err!(Config("log", "{e}.")))?;

	let console_layer = fmt::Layer::new()
		.with_ansi(config.log_colors)
		.with_thread_ids(config.log_thread_ids)
		.with_span_events(console_span_events)
		.fmt_fields(ConsoleFormat::new(config))
		.event_format(ConsoleFormat::new(config))
		.with_writer(ConsoleWriter::new(config));

	let (console_reload_filter, console_reload_handle) = reload::Layer::new(console_filter);

	reload_handles.add("console", Box::new(console_reload_handle));

	let cap_layer = capture::Layer::new(&cap_state);
	let subscriber = Registry::default()
		.with(console_layer.with_filter(console_reload_filter))
		.with(cap_layer);

	#[cfg(feature = "sentry_telemetry")]
	let subscriber = {
		let sentry_filter = EnvFilter::try_new(&config.sentry_filter)
			.map_err(|e| err!(Config("sentry_filter", "{e}.")))?;

		let sentry_layer = sentry_tracing::layer();
		let (sentry_reload_filter, sentry_reload_handle) = reload::Layer::new(sentry_filter);

		reload_handles.add("sentry", Box::new(sentry_reload_handle));
		subscriber.with(sentry_layer.with_filter(sentry_reload_filter))
	};

	#[cfg(feature = "perf_measurements")]
	let (subscriber, flame_guard) = {
		let (flame_layer, flame_guard) = if config.tracing_flame {
			let flame_filter = EnvFilter::try_new(&config.tracing_flame_filter)
				.map_err(|e| err!(Config("tracing_flame_filter", "{e}.")))?;

			let (flame_layer, flame_guard) =
				tracing_flame::FlameLayer::with_file(&config.tracing_flame_output_path)
					.map_err(|e| err!(Config("tracing_flame_output_path", "{e}.")))?;

			let flame_layer = flame_layer
				.with_empty_samples(false)
				.with_filter(flame_filter);

			(Some(flame_layer), Some(flame_guard))
		} else {
			(None, None)
		};

		#[cfg(tuwunel_disable)]
		let jaeger_filter = EnvFilter::try_new(&config.jaeger_filter)
			.map_err(|e| err!(Config("jaeger_filter", "{e}.")))?;

		#[cfg(tuwunel_disable)]
		let jaeger_layer = config.allow_jaeger.then(|| {
			opentelemetry::global::set_text_map_propagator(
				opentelemetry_jaeger::Propagator::new(),
			);

			let tracer = opentelemetry_jaeger::new_agent_pipeline()
				.with_auto_split_batch(true)
				.with_service_name("tuwunel")
				.install_batch(opentelemetry_sdk::runtime::Tokio)
				.expect("jaeger agent pipeline");

			let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

			let (jaeger_reload_filter, jaeger_reload_handle) =
				reload::Layer::new(jaeger_filter.clone());
			reload_handles.add("jaeger", Box::new(jaeger_reload_handle));

			Some(telemetry.with_filter(jaeger_reload_filter))
		});

		#[cfg(tuwunel_disable)]
		let subscriber = subscriber.with(flame_layer).with(jaeger_layer);
		let subscriber = subscriber.with(flame_layer);

		(subscriber, flame_guard)
	};

	#[cfg(not(feature = "perf_measurements"))]
	let flame_guard = None;

	let subscriber = Arc::new(subscriber);

	// Enable the tokio console. This is slightly kludgy because we're judggling
	// compile-time and runtime conditions to elide it, each of those changing the
	// subscriber's type.
	let (console_enabled, console_disabled_reason) = tokio_console_enabled(config);
	#[cfg(all(feature = "tokio_console", tokio_unstable, tuwunel_disable))]
	if console_enabled && config.log_global_default {
		let console_layer = console_subscriber::ConsoleLayer::builder()
			.with_default_env()
			.spawn();

		set_global_default(subscriber.clone().with(console_layer));
		return Ok((flame_guard, Log {
			reload: reload_handles,
			capture: cap_state,
			subscriber,
		}));
	}

	if config.log_global_default {
		set_global_default(subscriber.clone());
	}

	// If there's a reason the tokio console was disabled when it might be desired
	// we output that here after initializing logging
	if !console_enabled && !console_disabled_reason.is_empty() && config.log_global_default {
		debug_warn!("{console_disabled_reason}");
	}

	Ok((flame_guard, Logging {
		reload: reload_handles,
		capture: cap_state,
		subscriber,
	}))
}

fn tokio_console_enabled(config: &Config) -> (bool, &'static str) {
	if !cfg!(all(feature = "tokio_console", tokio_unstable, tuwunel_disable)) {
		return (false, "");
	}

	if cfg!(feature = "release_max_log_level") && !cfg!(debug_assertions) {
		return (
			false,
			"'tokio_console' feature and 'release_max_log_level' feature are incompatible.",
		);
	}

	if !config.tokio_console {
		return (false, "tokio console is available but disabled by the configuration.");
	}

	(true, "")
}

fn set_global_default<S: SubscriberExt + Send + Sync>(subscriber: S) {
	tracing::subscriber::set_global_default(subscriber)
		.expect("the global default tracing subscriber failed to be initialized");
}
