#![allow(dead_code)]

use std::{future::Future, sync::Arc};

use axum::Router;
use serde_json::json;
use tuwunel::{Args, Runtime, Server};
use tuwunel_core::Result;
use tuwunel_service::Services;

pub(crate) struct Harness {
	pub(crate) args: Args,
	runtime: Runtime,
}

impl Harness {
	pub(crate) fn new(name: &str, options: impl IntoIterator<Item = String>) -> Result<Self> {
		let mut args = Args::default_test(&[name, "fresh", "cleanup"]);
		args.maintenance = true;
		args.option.extend(options);
		let runtime = Runtime::new(Some(&args))?;

		Ok(Self { args, runtime })
	}

	pub(crate) fn discovery_server(&self, issuer: &'static str) -> Result<DiscoveryServer> {
		self.runtime.block_on(async move {
			let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
			let addr = listener.local_addr()?;
			let response = json!({
				"issuer": issuer,
				"authorization_endpoint": format!("{issuer}/authorize"),
				"token_endpoint": format!("{issuer}/token"),
				"userinfo_endpoint": format!("{issuer}/userinfo"),
				"revocation_endpoint": format!("{issuer}/revoke"),
				"introspection_endpoint": format!("{issuer}/introspect"),
			});
			let app = Router::new().fallback(move || {
				let response = response.clone();
				async move { axum::Json(response) }
			});
			let handle = tokio::spawn(async move {
				let _: std::io::Result<()> = axum::serve(listener, app).await;
			});

			Ok(DiscoveryServer {
				url: format!("http://{addr}/.well-known/openid-configuration"),
				handle,
			})
		})
	}

	/// Serve an arbitrary axum router on an ephemeral local port, for tests
	/// that need a richer identity-provider mock than `discovery_server`.
	pub(crate) fn mock_server(&self, app: Router) -> Result<MockServer> {
		self.runtime.block_on(async move {
			let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
			let addr = listener.local_addr()?;
			let handle = tokio::spawn(async move {
				let _: std::io::Result<()> = axum::serve(listener, app).await;
			});

			Ok(MockServer {
				base_url: format!("http://{addr}"),
				handle,
			})
		})
	}

	pub(crate) fn with_services<F, Fut>(&self, test: F) -> Result
	where
		F: FnOnce(Arc<Services>) -> Fut,
		Fut: Future<Output = Result>,
	{
		let server = Server::new(Some(&self.args), Some(&self.runtime))?;

		self.runtime.block_on(async {
			let services = tuwunel::async_start(&server).await?;
			let result = test(services.clone()).await;
			let shutdown_result = server.server.shutdown();
			drop(services);

			let stop_result = tuwunel::async_stop(&server).await;
			result.and(shutdown_result).and(stop_result)
		})
	}
}

pub(crate) struct DiscoveryServer {
	pub(crate) url: String,
	pub(crate) handle: tokio::task::JoinHandle<()>,
}

pub(crate) struct MockServer {
	pub(crate) base_url: String,
	pub(crate) handle: tokio::task::JoinHandle<()>,
}
