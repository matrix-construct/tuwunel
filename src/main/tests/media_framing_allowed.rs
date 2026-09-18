#![cfg(all(test, feature = "media_thumbnail"))]

//! Media responses with optional CSP restrictions enabled.
//!
//! Each non-default combination boots sequentially with logging disabled.
//! The baseline separately covers the default policy.

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

	const FRAMING: &str = concat!(
		"sandbox;default-src 'none';script-src 'none';font-src 'none';",
		"frame-ancestors 'none';form-action 'none';base-uri 'none';style-src 'unsafe-inline'",
	);

	const INLINE_STYLES: &str = concat!(
		"sandbox;default-src 'none';script-src 'none';font-src 'none';",
		"form-action 'none';base-uri 'none'",
	);

	const BOTH: &str = concat!(
		"sandbox;default-src 'none';script-src 'none';font-src 'none';",
		"frame-ancestors 'none';form-action 'none';base-uri 'none'",
	);

	#[test]
	fn restrictions_can_be_enabled_independently() -> Result {
		exercise(true, false, FRAMING)?;
		exercise(false, true, INLINE_STYLES)?;
		exercise(true, true, BOTH)
	}

	fn exercise(deny_framing: bool, deny_inline_styles: bool, policy: &str) -> Result {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();
		let args = [
			"address=[\"127.0.0.1\"]".to_owned(),
			format!("port={port}"),
			"listening=true".to_owned(),
			"allow_legacy_media=true".to_owned(),
			format!("media_deny_framing={deny_framing}"),
			format!("media_deny_inline_styles={deny_inline_styles}"),
			"log_enable=false".to_owned(),
		]
		.into_iter()
		.fold(Args::default_test(&["fresh", "cleanup"]), Args::with_option);

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;

		assert_eq!(server.server.config.media_deny_framing, deny_framing);
		assert_eq!(server.server.config.media_deny_inline_styles, deny_inline_styles);

		runtime.block_on(async {
			let services = async_start(&server).await?;
			let base = format!("http://127.0.0.1:{port}");

			drop(listener);

			let exercise = async {
				let checked = async {
					wait_until_ready(&services, &base).await?;
					register(&services, "mediaframing", TOKEN).await?;
					check(&services, &base, policy).await
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
