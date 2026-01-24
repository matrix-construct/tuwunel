use std::{
	fmt::Debug,
	sync::{Arc, atomic::Ordering},
	time::Duration,
};

use axum::{
	extract::State,
	response::{IntoResponse, Response},
};
use futures::FutureExt;
use http::{Method, StatusCode, Uri};
use tokio::time::sleep;
use tracing::Span;
use tuwunel_core::{Result, debug, debug_error, debug_warn, err, error, trace};
use tuwunel_service::Services;

#[tracing::instrument(
	name = "request",
	level = "debug",
	skip_all,
	err(Debug, level = "debug")
	fields(
		id = %services
			.server
			.metrics
			.requests_count
			.fetch_add(1, Ordering::Relaxed)
	)
)]
pub(crate) async fn handle(
	State(services): State<Arc<Services>>,
	req: http::Request<axum::body::Body>,
	next: axum::middleware::Next,
) -> Result<Response, StatusCode> {
	if !services.server.running() {
		debug_warn!(
			method = %req.method(),
			uri = %req.uri(),
			"unavailable pending shutdown"
		);

		return Err(StatusCode::SERVICE_UNAVAILABLE);
	}

	let uri = req.uri().clone();
	let method = req.method().clone();
	let services_ = services.clone();
	let parent = Span::current();
	let task = services.server.runtime().spawn(async move {
		tokio::select! {
			response = execute(&services_, req, next, &parent) => response,
			response = services_.server.until_shutdown()
				.then(|()| {
					let timeout = services_.server.config.client_shutdown_timeout;
					let timeout = Duration::from_secs(timeout);
					sleep(timeout)
				})
				.map(|()| StatusCode::SERVICE_UNAVAILABLE)
				.map(IntoResponse::into_response) => response,
		}
	});

	task.await
		.map_err(unhandled)
		.and_then(move |result| handle_result(&method, &uri, result))
}

#[tracing::instrument(
	name = "handle",
	level = "debug",
	parent = parent,
	skip_all,
	ret(level = "trace"),
)]
#[cfg_attr(not(debug_assertions), expect(unused_variables))]
async fn execute(
	// we made a safety contract that Services will not go out of scope
	// during the request; this ensures a reference is accounted for at
	// the base frame of the task regardless of its detachment.
	services: &Arc<Services>,
	req: http::Request<axum::body::Body>,
	next: axum::middleware::Next,
	parent: &Span,
) -> Response {
	#[cfg(debug_assertions)]
	services
		.server
		.metrics
		.requests_handle_active
		.fetch_add(1, Ordering::Relaxed);

	#[cfg(debug_assertions)]
	tuwunel_core::defer! {{
		_ = services.server
			.metrics
			.requests_handle_finished
			.fetch_add(1, Ordering::Relaxed);
		_ = services.server
			.metrics
			.requests_handle_active
			.fetch_sub(1, Ordering::Relaxed);
	}};

	next.run(req).await
}

fn handle_result(method: &Method, uri: &Uri, result: Response) -> Result<Response, StatusCode> {
	let status = result.status();
	let code = status.as_u16();
	let reason = status
		.canonical_reason()
		.unwrap_or("Unknown Reason");

	if status.is_server_error() {
		error!(method = ?method, uri = ?uri, "{code} {reason}");
	} else if status.is_client_error() {
		debug_error!(method = ?method, uri = ?uri, "{code} {reason}");
	} else if status.is_redirection() {
		debug!(method = ?method, uri = ?uri, "{code} {reason}");
	} else {
		trace!(method = ?method, uri = ?uri, "{code} {reason}");
	}

	if status == StatusCode::METHOD_NOT_ALLOWED {
		return Ok(err!(Request(Unrecognized("Method Not Allowed"))).into_response());
	}

	Ok(result)
}

#[cold]
fn unhandled<Error: Debug>(e: Error) -> StatusCode {
	error!("unhandled error or panic during request: {e:?}");

	StatusCode::INTERNAL_SERVER_ERROR
}
