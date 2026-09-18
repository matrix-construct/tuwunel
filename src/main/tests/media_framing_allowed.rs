#![cfg(all(test, feature = "media_thumbnail"))]

//! Media responses with the frame ancestry restriction enabled.
//!
//! This binary boots separately from the default-policy baseline so each
//! process initializes the server once.

#[expect(dead_code)] // This probe uses the shared fixtures without the thumbnail sweep.
mod media;
#[path = "media/policy.rs"]
mod policy;

#[cfg(test)]
mod tests {
	use std::net::TcpListener;

	use futures::future::join;
	use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
	use tuwunel_core::Result;

	use super::{
		media::{register, wait_until_ready},
		policy::{TOKEN, check},
	};

	const POLICY: &str = concat!(
		"sandbox;default-src 'none';script-src 'none';font-src 'none';",
		"frame-ancestors 'none';form-action 'none';base-uri 'none'",
	);

	#[test]
	fn framing_restriction_can_be_enabled() -> Result {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();
		let args = [
			"address=[\"127.0.0.1\"]".to_owned(),
			format!("port={port}"),
			"listening=true".to_owned(),
			"allow_legacy_media=true".to_owned(),
			"media_deny_framing=true".to_owned(),
			"log=\"error\"".to_owned(),
		]
		.into_iter()
		.fold(Args::default_test(&["fresh", "cleanup"]), Args::with_option);

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;

		assert!(server.server.config.media_deny_framing);

		runtime.block_on(async {
			let services = async_start(&server).await?;
			let base = format!("http://127.0.0.1:{port}");

			drop(listener);

			let exercise = async {
				let checked = async {
					wait_until_ready(&services, &base).await?;
					register(&services, "mediaframing", TOKEN).await?;
					check(&services, &base, POLICY).await
				}
				.await;

				server.server.shutdown().and(checked)
			};

			let (run_result, outcome) = join(async_run(&server), exercise).await;

			drop(services);
			async_stop(&server).await?;
			run_result?;

			outcome
		})
	}
}
