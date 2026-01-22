use std::{any::Any, sync::Arc, time::Duration};

use axum::{
	Router,
	extract::{DefaultBodyLimit, MatchedPath},
};
use axum_client_ip::SecureClientIpSource;
use http::{
	HeaderValue, Method, StatusCode,
	header::{self, HeaderName},
	uri::PathAndQuery,
};
use tower::ServiceBuilder;
use tower_http::{
	catch_panic::CatchPanicLayer,
	cors::{AllowOrigin, CorsLayer},
	sensitive_headers::SetSensitiveHeadersLayer,
	set_header::SetResponseHeaderLayer,
	timeout::{RequestBodyTimeoutLayer, ResponseBodyTimeoutLayer, TimeoutLayer},
	trace::{DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::Level;
use tuwunel_api::router::state::Guard;
use tuwunel_core::{Result, Server, debug, error};
use tuwunel_service::Services;

use crate::{request, router};

const TUWUNEL_CSP: &[&str; 5] = &[
	"default-src 'none'",
	"frame-ancestors 'none'",
	"form-action 'none'",
	"base-uri 'none'",
	"sandbox",
];

const TUWUNEL_PERMISSIONS_POLICY: &[&str; 2] = &["interest-cohort=()", "browsing-topics=()"];

pub(crate) fn build(services: &Arc<Services>) -> Result<(Router, Guard)> {
	let server = &services.server;
	let layers = ServiceBuilder::new();

	#[cfg(feature = "sentry_telemetry")]
	let layers = layers.layer(sentry_tower::NewSentryLayer::<http::Request<_>>::new_from_top());

	#[cfg(any(
		feature = "zstd_compression",
		feature = "gzip_compression",
		feature = "brotli_compression"
	))]
	let layers = layers.layer(compression_layer(server));

	let services_ = services.clone();
	let layers = layers
		.layer(SetSensitiveHeadersLayer::new([header::AUTHORIZATION]))
		.layer(
			TraceLayer::new_for_http()
				.make_span_with(tracing_span::<_>)
				.on_failure(DefaultOnFailure::new().level(Level::ERROR))
				.on_request(DefaultOnRequest::new().level(Level::TRACE))
				.on_response(DefaultOnResponse::new().level(Level::DEBUG)),
		)
		.layer(axum::middleware::from_fn_with_state(Arc::clone(services), request::handle))
		.layer(SecureClientIpSource::ConnectInfo.into_extension())
		.layer(ResponseBodyTimeoutLayer::new(Duration::from_secs(
			server.config.client_response_timeout,
		)))
		.layer(RequestBodyTimeoutLayer::new(Duration::from_secs(
			server.config.client_receive_timeout,
		)))
		.layer(TimeoutLayer::with_status_code(
			StatusCode::REQUEST_TIMEOUT,
			Duration::from_secs(server.config.client_request_timeout),
		))
		.layer(SetResponseHeaderLayer::if_not_present(
			// https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Origin-Agent-Cluster
			HeaderName::from_static("origin-agent-cluster"),
			HeaderValue::from_static("?1"),
		))
		.layer(SetResponseHeaderLayer::if_not_present(
			header::X_CONTENT_TYPE_OPTIONS,
			HeaderValue::from_static("nosniff"),
		))
		.layer(SetResponseHeaderLayer::if_not_present(
			header::X_XSS_PROTECTION,
			HeaderValue::from_static("0"),
		))
		.layer(SetResponseHeaderLayer::if_not_present(
			header::X_FRAME_OPTIONS,
			HeaderValue::from_static("DENY"),
		))
		.layer(SetResponseHeaderLayer::if_not_present(
			HeaderName::from_static("permissions-policy"),
			HeaderValue::from_str(&TUWUNEL_PERMISSIONS_POLICY.join(","))?,
		))
		.layer(SetResponseHeaderLayer::if_not_present(
			header::CONTENT_SECURITY_POLICY,
			HeaderValue::from_str(&TUWUNEL_CSP.join(";"))?,
		))
		.layer(cors_layer(server))
		.layer(body_limit_layer(server))
		.layer(CatchPanicLayer::custom(move |panic| catch_panic(panic, services_.clone())));

	let (router, guard) = router::build(services);
	Ok((router.layer(layers), guard))
}

#[cfg(any(
	feature = "zstd_compression",
	feature = "gzip_compression",
	feature = "brotli_compression"
))]
fn compression_layer(server: &Server) -> tower_http::compression::CompressionLayer {
	let mut compression_layer = tower_http::compression::CompressionLayer::new();

	#[cfg(feature = "zstd_compression")]
	{
		compression_layer = if server.config.zstd_compression {
			compression_layer.zstd(true)
		} else {
			compression_layer.no_zstd()
		};
	};

	#[cfg(feature = "gzip_compression")]
	{
		compression_layer = if server.config.gzip_compression {
			compression_layer.gzip(true)
		} else {
			compression_layer.no_gzip()
		};
	};

	#[cfg(feature = "brotli_compression")]
	{
		compression_layer = if server.config.brotli_compression {
			compression_layer.br(true)
		} else {
			compression_layer.no_br()
		};
	};

	compression_layer
}

fn cors_layer(server: &Server) -> CorsLayer {
	const METHODS: [Method; 7] = [
		Method::DELETE,
		Method::GET,
		Method::HEAD,
		Method::OPTIONS,
		Method::PATCH,
		Method::POST,
		Method::PUT,
	];

	let headers: [HeaderName; 5] = [
		header::ACCEPT,
		header::AUTHORIZATION,
		header::CONTENT_TYPE,
		header::ORIGIN,
		HeaderName::from_lowercase(b"x-requested-with").unwrap(),
	];

	let allow_origin_list = server
		.config
		.access_control_allow_origin
		.iter()
		.map(AsRef::as_ref)
		.map(HeaderValue::from_str)
		.filter_map(Result::ok);

	let allow_origin = if !server
		.config
		.access_control_allow_origin
		.is_empty()
	{
		AllowOrigin::list(allow_origin_list)
	} else {
		AllowOrigin::any()
	};

	CorsLayer::new()
		.max_age(Duration::from_secs(86400))
		.allow_methods(METHODS)
		.allow_headers(headers)
		.allow_origin(allow_origin)
}

fn body_limit_layer(server: &Server) -> DefaultBodyLimit {
	DefaultBodyLimit::max(server.config.max_request_size)
}

#[tracing::instrument(name = "panic", level = "error", skip_all)]
#[expect(clippy::needless_pass_by_value)]
fn catch_panic(
	err: Box<dyn Any + Send + 'static>,
	services: Arc<Services>,
) -> http::Response<http_body_util::Full<bytes::Bytes>> {
	services
		.server
		.metrics
		.requests_panic
		.fetch_add(1, std::sync::atomic::Ordering::Release);

	let details = match err.downcast_ref::<String>() {
		| Some(s) => s.clone(),
		| _ => match err.downcast_ref::<&str>() {
			| Some(s) => (*s).to_owned(),
			| _ => "Unknown internal server error occurred.".to_owned(),
		},
	};

	error!("{details:#}");
	let body = serde_json::json!({
		"errcode": "M_UNKNOWN",
		"error": "M_UNKNOWN: Internal server error occurred",
		"details": details,
	});

	http::Response::builder()
		.status(StatusCode::INTERNAL_SERVER_ERROR)
		.header(header::CONTENT_TYPE, "application/json")
		.body(http_body_util::Full::from(body.to_string()))
		.expect("Failed to create response for our panic catcher?")
}

fn tracing_span<T>(request: &http::Request<T>) -> tracing::Span {
	let path = request
		.extensions()
		.get::<MatchedPath>()
		.map_or_else(|| request_path_str(request), truncated_matched_path);

	tracing::span! {
		parent: None,
		debug::INFO_SPAN_LEVEL,
		"router",
		method = %request.method(),
		%path,
	}
}

fn request_path_str<T>(request: &http::Request<T>) -> &str {
	request
		.uri()
		.path_and_query()
		.map(PathAndQuery::as_str)
		.unwrap_or("/")
}

fn truncated_matched_path(path: &MatchedPath) -> &str {
	path.as_str()
		.rsplit_once('{')
		.map_or(path.as_str(), |path| path.0.strip_suffix('/').unwrap_or(path.0))
}
