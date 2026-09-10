use std::{
	convert::Infallible,
	fmt::Debug,
	sync::{Arc, atomic::Ordering},
	time::Duration,
};

use axum::{
	extract::{MatchedPath, Request},
	response::{IntoResponse, Response},
};
use futures::FutureExt;
use http::{Method, StatusCode, Uri};
use ruma::api::error::ErrorKind;
use tokio::{sync::Notify, task, time::sleep};
use tower::{Service, ServiceExt};
use tracing::{Span, field::Empty};
use tuwunel_core::{
	Error, Result, debug, debug_error, debug_warn, defer, error, trace, utils::SanitizedUri,
};
use tuwunel_service::Services;

#[tracing::instrument(
	name = "request",
	level = "debug",
	skip_all,
	err(Debug, level = "debug")
	fields(
		task = %task::id(),
		id = %services
			.server
			.metrics
			.requests_count
			.fetch_add(1, Ordering::Relaxed),
		origin = Empty,
		user_id = Empty,
		device_id = Empty,
	)
)]
pub(crate) async fn handle<S>(
	services: Arc<Services>,
	mut req: Request,
	inner: S,
) -> Result<Response, StatusCode>
where
	S: Service<Request, Error = Infallible> + Send + 'static,
	S::Response: IntoResponse,
	S::Future: Send + 'static,
{
	let matched_path = req.extensions().get::<MatchedPath>().cloned();

	if !services.server.is_running() {
		let uri = matched_path.as_ref().map_or_else(
			|| SanitizedUri::new(req.uri()),
			|path| SanitizedUri::with_path(req.uri(), path.as_str()),
		);

		debug_warn!(
			method = %req.method(),
			%uri,
			"unavailable pending shutdown"
		);

		return Err(StatusCode::SERVICE_UNAVAILABLE);
	}

	let uri = req.uri().clone();
	let method = req.method().clone();
	let parent = Span::current();
	req.extensions_mut().insert(parent.clone());

	let response = match method {
		| Method::PUT | Method::POST | Method::DELETE | Method::PATCH =>
			spawn_execute(services, req, inner, parent).await?,
		| _ => execute(&services, req, inner, &parent).await,
	};

	handle_result(&method, &uri, matched_path.as_ref(), response)
}

async fn spawn_execute<S>(
	services: Arc<Services>,
	mut req: Request,
	inner: S,
	parent: Span,
) -> Result<Response, StatusCode>
where
	S: Service<Request, Error = Infallible> + Send + 'static,
	S::Response: IntoResponse,
	S::Future: Send + 'static,
{
	let detached = Arc::new(Notify::new());
	req.extensions_mut().insert(detached.clone());

	let task = services
		.clone()
		.server
		.runtime()
		.spawn(async move {
			tokio::select! {
				response = execute(&services, req, inner, &parent) => response,
				response = services.server.until_shutdown()
					.then(|()| {
						let timeout = services.config.client_shutdown_timeout;
						sleep(Duration::from_secs(timeout))
					})
					.map(|()| StatusCode::SERVICE_UNAVAILABLE)
					.map(IntoResponse::into_response) => response,
			}
		});

	let abort = task.abort_handle();
	defer! {{
		if !abort.is_finished() {
			debug_warn!(
				task = ?abort.id(),
				"Client disconnected; detached request."
			);

			detached.notify_one();
		}
	}};

	task.await.map_err(unhandled)
}

#[tracing::instrument(
	name = "handle",
	level = "debug",
	parent = parent,
	skip_all,
	ret(level = "trace"),
	fields(
		task = %task::id(),
	)
)]
#[cfg_attr(not(debug_assertions), expect(unused_variables))]
async fn execute<S>(
	// we made a safety contract that Services will not go out of scope
	// during the request; this ensures a reference is accounted for at
	// the base frame of the task regardless of its detachment.
	services: &Arc<Services>,
	req: Request,
	inner: S,
	parent: &Span,
) -> Response
where
	S: Service<Request, Error = Infallible>,
	S::Response: IntoResponse,
{
	#[cfg(debug_assertions)]
	services
		.server
		.metrics
		.requests_handle_active
		.fetch_add(1, Ordering::Relaxed);

	#[cfg(debug_assertions)]
	defer! {{
		_ = services.server
			.metrics
			.requests_handle_finished
			.fetch_add(1, Ordering::Relaxed);
		_ = services.server
			.metrics
			.requests_handle_active
			.fetch_sub(1, Ordering::Relaxed);
	}};

	inner
		.oneshot(req)
		.map(IntoResponse::into_response)
		.await
}

fn handle_result(
	method: &Method,
	uri: &Uri,
	matched_path: Option<&MatchedPath>,
	result: Response,
) -> Result<Response, StatusCode> {
	let status = result.status();
	let code = status.as_u16();
	let reason = status
		.canonical_reason()
		.unwrap_or("Unknown Reason");

	let uri = matched_path.map_or_else(
		|| SanitizedUri::new(uri),
		|path| SanitizedUri::with_path(uri, path.as_str()),
	);

	match status {
		| status if status.is_redirection() =>
			debug!(method = ?method, %uri, status = code, %reason, "request complete"),
		| status if status.is_server_error() =>
			error!(method = ?method, %uri, status = code, %reason, "request complete"),
		| status if status.is_client_error() => {
			debug_error!(method = ?method, %uri, status = code, %reason, "request complete");
		},
		| _ => trace!(method = ?method, %uri, status = code, %reason, "request complete"),
	}

	if status == StatusCode::METHOD_NOT_ALLOWED {
		return Ok(Error::Request(
			ErrorKind::Unrecognized,
			"Method Not Allowed".into(),
			StatusCode::METHOD_NOT_ALLOWED,
		)
		.into_response());
	}

	Ok(result)
}

#[cold]
fn unhandled<Error: Debug>(e: Error) -> StatusCode {
	error!(error = ?e, "unhandled error or panic during request");

	StatusCode::INTERNAL_SERVER_ERROR
}
