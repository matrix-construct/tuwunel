use std::fmt::Debug;

use axum::{
	Router,
	extract::FromRequestParts,
	response::IntoResponse,
	routing::{MethodFilter, on},
};
use futures::{Future, TryFutureExt};
use http::Method;
use ruma::api::IncomingRequest;
use tuwunel_core::{Result, trace};

use super::{Ruma, RumaResponse, State};

pub(in super::super) trait RumaHandler<T> {
	fn add_route(&'static self, router: Router<State>, path: &str) -> Router<State>;
	fn add_routes(&'static self, router: Router<State>) -> Router<State>;
}

pub(in super::super) trait RouterExt {
	fn ruma_route<H, T>(self, handler: &'static H) -> Self
	where
		H: RumaHandler<T>;
}

impl RouterExt for Router<State> {
	fn ruma_route<H, T>(self, handler: &'static H) -> Self
	where
		H: RumaHandler<T>,
	{
		handler.add_routes(self)
	}
}

macro_rules! ruma_handler {
	( $($tx:ident),* $(,)? ) => {
		#[allow(clippy::allow_attributes)]
		#[allow(non_snake_case)]
		impl<Err, Req, Fut, Fun, $($tx,)*> RumaHandler<($($tx,)* Ruma<Req>,)> for Fun
		where
			Fun: Fn($($tx,)* Ruma<Req>,) -> Fut + Send + Sync + 'static,
			Fut: Future<Output = Result<Req::OutgoingResponse, Err>> + Send,
			Req: IncomingRequest + Debug + Send + Sync + 'static,
			Err: IntoResponse + Debug + Send,
			<Req as IncomingRequest>::OutgoingResponse: Debug + Send,
			$( $tx: FromRequestParts<State> + Send + Sync + 'static, )*
		{
			fn add_routes(&'static self, router: Router<State>) -> Router<State> {
				Req::METADATA
					.history
					.all_paths()
					.fold(router, |router, path| self.add_route(router, path))
			}

			fn add_route(&'static self, router: Router<State>, path: &str) -> Router<State> {
				let method = method_to_filter(&Req::METADATA.method);
				let action = |$($tx,)* req| {
					self($($tx,)* req)
						.inspect_ok(|result| trace!(?result))
						.map_ok(RumaResponse)
				};

				router.route(path, on(method, action))
			}
		}
	}
}
ruma_handler!();
ruma_handler!(T1);
ruma_handler!(T1, T2);
ruma_handler!(T1, T2, T3);
ruma_handler!(T1, T2, T3, T4);

const fn method_to_filter(method: &Method) -> MethodFilter {
	match *method {
		| Method::DELETE => MethodFilter::DELETE,
		| Method::GET => MethodFilter::GET,
		| Method::HEAD => MethodFilter::HEAD,
		| Method::OPTIONS => MethodFilter::OPTIONS,
		| Method::PATCH => MethodFilter::PATCH,
		| Method::POST => MethodFilter::POST,
		| Method::PUT => MethodFilter::PUT,
		| Method::TRACE => MethodFilter::TRACE,
		| _ => panic!("Unsupported HTTP method"),
	}
}
