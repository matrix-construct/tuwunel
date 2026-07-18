#![cfg(feature = "sentry_telemetry")]

use std::{
	str::FromStr,
	sync::{Arc, OnceLock},
};

use reqwest::{Certificate, Client, ClientBuilder, Proxy};
use sentry::{
	Breadcrumb, ClientOptions, Level, Transport, TransportOptions,
	transports::ReqwestHttpTransportOptions,
	types::{
		Dsn,
		protocol::v7::{Context, Event},
	},
};
use tuwunel_core::{config::Config, debug, trace};

static SEND_PANIC: OnceLock<bool> = OnceLock::new();
static SEND_ERROR: OnceLock<bool> = OnceLock::new();

pub(crate) fn init(config: &Config) -> Option<sentry::ClientInitGuard> {
	config
		.sentry
		.then(|| sentry::init(options(config)))
}

fn options(config: &Config) -> ClientOptions {
	SEND_PANIC
		.set(config.sentry_send_panic)
		.expect("SEND_PANIC was not previously set");
	SEND_ERROR
		.set(config.sentry_send_error)
		.expect("SEND_ERROR was not previously set");

	let dsn = config
		.sentry_endpoint
		.as_ref()
		.expect("init_sentry should only be called if sentry is enabled and this is not None")
		.as_str();

	let server_name = config
		.sentry_send_server_name
		.then(|| config.server_name.to_string().into());

	ClientOptions {
		dsn: Some(Dsn::from_str(dsn).expect("sentry_endpoint must be a valid URL")),
		server_name,
		traces_sample_rate: config.sentry_traces_sample_rate,
		debug: cfg!(debug_assertions),
		release: sentry::release_name!(),
		user_agent: tuwunel_core::version::user_agent().into(),
		attach_stacktrace: config.sentry_attach_stacktrace,
		before_send: Some(Arc::new(before_send)),
		before_breadcrumb: Some(Arc::new(before_breadcrumb)),
		transport: Some(Arc::new(build_transport)),
		..Default::default()
	}
}

// Sentry's default reqwest transport builds a Client with no extra roots,
// which fails on minimal images where `rustls-platform-verifier` finds an
// empty system store. Mirror `service::client::base()` and supply webpki
// roots so the verifier has at least one source of trust.
fn build_transport(options: &ClientOptions) -> Arc<dyn Transport> {
	let webpki = webpki_root_certs::TLS_SERVER_ROOT_CERTS
		.iter()
		.map(|der| Certificate::from_der(der).expect("certificate must be valid der encoding"));

	let proxies = [
		options
			.http_proxy
			.as_ref()
			.and_then(|url| Proxy::http(url.as_ref()).ok()),
		options
			.https_proxy
			.as_ref()
			.and_then(|url| Proxy::https(url.as_ref()).ok()),
	];

	let builder = Client::builder().danger_accept_invalid_certs(options.accept_invalid_certs);

	#[cfg(target_os = "android")]
	let builder = builder.tls_certs_only(webpki);

	#[cfg(not(target_os = "android"))]
	let builder = builder.tls_certs_merge(webpki);

	let client = proxies
		.into_iter()
		.flatten()
		.fold(builder, ClientBuilder::proxy)
		.build()
		.expect("reqwest client must build for sentry transport");

	let transport_options = TransportOptions::try_from_client_options(options)
		.expect("sentry client options must have a DSN");

	let transport = ReqwestHttpTransportOptions::from(transport_options)
		.with_client(client)
		.build();

	Arc::new(transport)
}

fn before_send(event: Event<'static>) -> Option<Event<'static>> {
	if event.exception.iter().any(|e| e.ty == "panic") && !SEND_PANIC.get().unwrap_or(&true) {
		return None;
	}

	if event.level == Level::Error {
		if !SEND_ERROR.get().unwrap_or(&true) {
			return None;
		}

		if cfg!(debug_assertions) {
			return None;
		}

		//NOTE: we can enable this to specify error!(sentry = true, ...)
		if let Some(Context::Other(context)) = event.contexts.get("Rust Tracing Fields")
			&& !context.contains_key("sentry")
		{
			//return None;
		}
	}

	if event.level == Level::Fatal {
		trace!("{event:#?}");
	}

	debug!("Sending sentry event: {event:?}");
	Some(event)
}

fn before_breadcrumb(crumb: Breadcrumb) -> Option<Breadcrumb> {
	if crumb.ty == "log" && crumb.level == Level::Debug {
		return None;
	}

	trace!("Sentry breadcrumb: {crumb:?}");
	Some(crumb)
}
