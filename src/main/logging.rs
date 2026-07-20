use std::sync::Arc;

use tracing::{Subscriber, subscriber::NoSubscriber};
use tracing_subscriber::{
	EnvFilter, Layer, Registry, fmt, layer::SubscriberExt, registry::LookupSpan, reload,
};
use tuwunel_core::{
	Result,
	config::Config,
	debug_warn, err,
	log::{ConsoleFormat, ConsoleWriter, LogLevelReloadHandles, Logging, capture, fmt_span},
	result::UnwrapOrErr,
};
#[cfg(feature = "perf_measurements")]
use {
	opentelemetry::trace::TracerProvider as _,
	opentelemetry_otlp::SpanExporter,
	opentelemetry_sdk::{
		Resource, propagation::TraceContextPropagator, trace::SdkTracerProvider,
	},
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

	let cap_layer = capture::Layer::new(&cap_state);
	let subscriber = Registry::default()
		.with(console_layer(config, &reload_handles)?)
		.with(cap_layer);

	#[cfg(feature = "sentry_telemetry")]
	let subscriber = subscriber.with(sentry_layer(config, &reload_handles)?);

	#[cfg(feature = "perf_measurements")]
	let (subscriber, flame_guard) = {
		let (flame_layer, flame_guard) = tracing_flame_layer(config)?;
		let jaeger_layer = opentelemetry_layer(config, &reload_handles)?;
		let subscriber = subscriber.with(flame_layer).with(jaeger_layer);

		(subscriber, flame_guard)
	};

	#[cfg(not(feature = "perf_measurements"))]
	let flame_guard = None;

	let (console_enabled, console_disabled_reason) = tokio_console_enabled(config);

	#[cfg(all(feature = "tokio_console", tokio_unstable))]
	let subscriber = subscriber.with(tokio_console_layer(config, console_enabled));

	let subscriber = Arc::new(subscriber);

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

fn console_layer<S>(
	config: &Config,
	reload_handles: &LogLevelReloadHandles,
) -> Result<impl Layer<S>>
where
	S: Subscriber + for<'a> LookupSpan<'a> + 'static,
{
	let span_events = fmt_span::from_str(&config.log_span_events).unwrap_or_err();

	let filter = EnvFilter::builder()
		.with_regex(config.log_filter_regex)
		.parse(&config.log)
		.map_err(|e| err!(Config("log", "{e}.")))?;

	let layer = fmt::Layer::new()
		.with_ansi(config.log_colors)
		.with_thread_ids(config.log_thread_ids)
		.with_span_events(span_events)
		.fmt_fields(ConsoleFormat::new(config))
		.event_format(ConsoleFormat::new(config))
		.with_writer(ConsoleWriter::new(config));

	let (reload_filter, reload_handle) = reload::Layer::new(filter);

	reload_handles.add("console", Box::new(reload_handle));
	Ok(layer.with_filter(reload_filter))
}

#[cfg(feature = "sentry_telemetry")]
fn sentry_layer<S>(
	config: &Config,
	reload_handles: &LogLevelReloadHandles,
) -> Result<impl Layer<S>>
where
	S: Subscriber + for<'a> LookupSpan<'a> + 'static,
{
	let filter = EnvFilter::try_new(&config.sentry_filter)
		.map_err(|e| err!(Config("sentry_filter", "{e}.")))?;

	let layer = sentry_tracing::layer();
	let (reload_filter, reload_handle) = reload::Layer::new(filter);

	reload_handles.add("sentry", Box::new(reload_handle));
	Ok(layer.with_filter(reload_filter))
}

#[cfg(feature = "perf_measurements")]
fn tracing_flame_layer<S>(config: &Config) -> Result<(Option<impl Layer<S>>, TracingFlameGuard)>
where
	S: Subscriber + for<'a> LookupSpan<'a> + 'static,
{
	if !config.tracing_flame {
		return Ok((None, None));
	}

	let filter = EnvFilter::try_new(&config.tracing_flame_filter)
		.map_err(|e| err!(Config("tracing_flame_filter", "{e}.")))?;

	let (layer, guard) = tracing_flame::FlameLayer::with_file(&config.tracing_flame_output_path)
		.map_err(|e| err!(Config("tracing_flame_output_path", "{e}.")))?;

	let layer = layer
		.with_empty_samples(false)
		.with_filter(filter);

	Ok((Some(layer), Some(guard)))
}

#[cfg(feature = "perf_measurements")]
fn opentelemetry_layer<S>(
	config: &Config,
	reload_handles: &LogLevelReloadHandles,
) -> Result<Option<impl Layer<S>>>
where
	S: Subscriber + for<'a> LookupSpan<'a> + 'static,
{
	let filter = EnvFilter::try_new(&config.jaeger_filter)
		.map_err(|e| err!(Config("jaeger_filter", "{e}.")))?;

	let layer = config.allow_jaeger.then(|| {
		opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

		let exporter = SpanExporter::builder()
			.with_tonic()
			.build()
			.expect("otlp span exporter");

		let resource = Resource::builder()
			.with_service_name("tuwunel")
			.build();

		let provider = SdkTracerProvider::builder()
			.with_batch_exporter(exporter)
			.with_resource(resource)
			.build();

		let tracer = provider.tracer("tuwunel");
		let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

		let (reload_filter, reload_handle) = reload::Layer::new(filter.clone());

		reload_handles.add("jaeger", Box::new(reload_handle));
		telemetry.with_filter(reload_filter)
	});

	Ok(layer)
}

fn tokio_console_enabled(config: &Config) -> (bool, &'static str) {
	if !cfg!(all(feature = "tokio_console", tokio_unstable)) {
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

#[cfg(all(feature = "tokio_console", tokio_unstable))]
fn tokio_console_layer<S>(config: &Config, console_enabled: bool) -> impl Layer<S>
where
	S: Subscriber + for<'a> LookupSpan<'a> + 'static,
{
	(console_enabled && config.log_global_default).then(|| {
		console_subscriber::ConsoleLayer::builder()
			.with_default_env()
			.spawn()
	})
}

fn set_global_default<S: SubscriberExt + Send + Sync>(subscriber: S) {
	tracing::subscriber::set_global_default(subscriber)
		.expect("the global default tracing subscriber failed to be initialized");
}
