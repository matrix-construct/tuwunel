use std::{any::Any, fmt::Debug};

use axum::{
	Router,
	body::Body,
	extract::{FromRequest, FromRequestParts},
	handler::Handler,
	response::{IntoResponse, Response},
	routing::{MethodFilter, on},
};
use futures::future::BoxFuture;
use http::{Method, Request};
use ruma::api::{IncomingRequest, path_builder::PathBuilder};
use tuwunel_core::{Result, trace};

use super::{Ruma, RumaResponse, State, auth::AuthDispatch};

pub(in super::super) trait RumaHandler<T> {
	fn add_route(&'static self, router: Router<State>, path: &str) -> Router<State>;
	fn add_routes(&'static self, router: Router<State>) -> Router<State>;
	fn call_route(handler: RouteHandler, state: State, request: Request<Body>) -> RouteResponse;
}

pub(in super::super) trait RouterExt {
	fn ruma_route<H, T>(self, handler: &'static H) -> Self
	where
		H: RumaHandler<T>;
}

/// A route handler reduced to one fn-pointer shape: axum's generic routing
/// stack instantiates once for this type instead of once per endpoint.
#[derive(Clone, Copy)]
struct Route {
	call: RouteCall,
	handler: RouteHandler,
}

type RouteCall = fn(RouteHandler, State, Request<Body>) -> RouteResponse;
type RouteHandler = &'static (dyn Any + Send + Sync);
type RouteResponse = BoxFuture<'static, Response>;

impl RouterExt for Router<State> {
	fn ruma_route<H, T>(self, handler: &'static H) -> Self
	where
		H: RumaHandler<T>,
	{
		handler.add_routes(self)
	}
}

impl Handler<(), State> for Route {
	type Future = RouteResponse;

	fn call(self, request: Request<Body>, state: State) -> Self::Future {
		(self.call)(self.handler, state, request)
	}
}

macro_rules! ruma_handler {
	( $($tx:ident),* $(,)? ) => {
		#[allow(clippy::allow_attributes, non_snake_case)]
		impl<Err, Req, Fut, Fun, $($tx,)*> RumaHandler<($($tx,)* Ruma<Req>,)> for Fun
		where
			Fun: Fn($($tx,)* Ruma<Req>,) -> Fut + Send + Sync + 'static,
			Fut: Future<Output = Result<Req::OutgoingResponse, Err>> + Send,
			Req: IncomingRequest + Debug + Send + Sync + 'static,
			Req::Authentication: AuthDispatch,
			Err: IntoResponse + Debug + Send,
			<Req as IncomingRequest>::OutgoingResponse: Debug + Send,
			$( $tx: FromRequestParts<State> + Send + Sync + 'static, )*
		{
			fn add_routes(&'static self, router: Router<State>) -> Router<State> {
				Req::PATH_BUILDER
					.all_paths()
					.fold(router, |router, path| self.add_route(router, path))
			}

			fn add_route(&'static self, router: Router<State>, path: &str) -> Router<State> {
				let route = Route { handler: self, call: Self::call_route };

				router.route(path, on(method_to_filter(&Req::METHOD), route))
			}

			fn call_route(
				handler: RouteHandler,
				state: State,
				request: Request<Body>,
			) -> RouteResponse {
				let handler: &'static Fun = handler
					.downcast_ref()
					.expect("route handler matches the type it registered with");

				let response = async move {
					#[allow(unused_mut)]
					let (mut parts, body) = request.into_parts();
					$(
						let $tx = match $tx::from_request_parts(&mut parts, &state).await {
							| Err(error) => return error.into_response(),
							| Ok(value) => value,
						};
					)*

					let request = Request::from_parts(parts, body);
					let args = match Ruma::<Req>::from_request(request, &state).await {
						| Err(error) => return error.into_response(),
						| Ok(args) => args,
					};

					match handler($($tx,)* args).await.inspect(|response| trace!(?response)) {
						| Err(error) => error.into_response(),
						| Ok(response) => RumaResponse(response).into_response(),
					}
				};

				Box::pin(response)
			}
		}
	}
}
ruma_handler!();
ruma_handler!(T1);
ruma_handler!(T1, T2);
ruma_handler!(T1, T2, T3);
ruma_handler!(T1, T2, T3, T4);

fn method_to_filter(method: &Method) -> MethodFilter {
	match method {
		| &Method::DELETE => MethodFilter::DELETE,
		| &Method::GET => MethodFilter::GET,
		| &Method::HEAD => MethodFilter::HEAD,
		| &Method::OPTIONS => MethodFilter::OPTIONS,
		| &Method::PATCH => MethodFilter::PATCH,
		| &Method::POST => MethodFilter::POST,
		| &Method::PUT => MethodFilter::PUT,
		| &Method::TRACE => MethodFilter::TRACE,
		| _ => panic!("Unsupported HTTP method"),
	}
}
