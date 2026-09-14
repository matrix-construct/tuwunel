#![cfg(all(test, feature = "direct_tls", feature = "media_thumbnail"))]

//! What a peer is served when it asks us for a thumbnail.
//!
//! The federation thumbnail endpoint is a third handler over the same service
//! as the two client ones, and the only one no plain HTTP call can reach,
//! since a peer's request is signed. The server booted here has its own
//! address as its name so it federates with itself, then asks every case twice,
//! once as a peer and once as a client, putting both answers on adjacent lines.
//!
//! Regenerate deliberately, never to make a red run green:
//! `INSTA_FORCE_UPDATE=1 cargo +nightly test --features direct_tls --test
//! media_federation`.

mod media;

// clippy's tests_outside_test_module does not see the compound cfg above as a
// test module, so the wrapper is load-bearing rather than ceremony
#[cfg(test)]
mod tests {
	use std::{fmt::Display, net::TcpListener, path::PathBuf, time::Duration};

	use futures::{StreamExt, TryStreamExt, future::join};
	use insta::{assert_snapshot, with_settings};
	use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
	use tuwunel_core::{
		Result,
		ruma::{
			ServerName, UInt,
			api::federation::authenticated_media::{
				FileOrLocation,
				get_content_thumbnail::v1::{Request, Response},
			},
			media::Method,
		},
		utils::stream::IterStream,
	};
	use tuwunel_service::Services;

	use super::media::{
		Answer, Ask, CORPUS, DatabasePath, Source, asks, describe, register, row, thumbnail,
		upload, wait_until_ready,
	};

	const TOKEN: &str = "media-federation-harness-access-token";

	const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";

	const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";

	const TIMEOUT: Duration = Duration::from_secs(10);

	/// Sweeps the corpus over the federation and client thumbnail endpoints.
	///
	/// A peer and a client reach the same service through separate handlers,
	/// and the federation one is where the `animated` parameter has the
	/// furthest to travel, so a row that moves for one surface and not the
	/// other localizes the change to a handler rather than to the service
	/// beneath both.
	#[test]
	fn federation_thumbnail_baseline() -> Result {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();

		let db_path = DatabasePath(Args::test_database_path("media-federation"));

		let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
		let certificate = manifest.join(CERTIFICATE);
		let private_key = manifest.join(PRIVATE_KEY);

		let args = [
			format!("database_path={:?}", db_path.0),
			format!("server_name=\"127.0.0.1:{port}\""),
			"address=[\"127.0.0.1\"]".to_owned(),
			format!("port={port}"),
			"listening=true".to_owned(),
			"federation_loopback=true".to_owned(),
			"ip_range_denylist=[]".to_owned(),
			"allow_invalid_tls_certificates=true".to_owned(),
			format!("tls.certs={certificate:?}"),
			format!("tls.key={private_key:?}"),
			"log=\"error\"".to_owned(),
		]
		.into_iter()
		.fold(Args::default_test(&["fresh", "cleanup"]), Args::with_option);

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;
		let result = runtime.block_on(async {
			let services = async_start(&server).await?;
			let base = format!("https://127.0.0.1:{port}");

			drop(listener);

			let exercise = async {
				let report = sweep(&services, &base).await;
				let shutdown = server.server.shutdown();

				// a report gathered from a server that then failed to stop is not
				// evidence of anything, so the shutdown outranks it
				shutdown.and(report)
			};

			let (run_result, outcome) = join(async_run(&server), exercise).await;

			drop(services);
			async_stop(&server).await?;
			run_result?;

			outcome
		});

		drop(runtime);

		let report = result?;

		with_settings!({
			description => "Federation thumbnail baseline",
			snapshot_suffix => "federation_thumbnail",
			omit_expression => true,
		}, {
			assert_snapshot!(report);
		});

		Ok(())
	}

	/// Runs every case over both surfaces and renders the report.
	///
	/// Each surface gets its own upload of the same picture, so neither can
	/// observe a thumbnail variant the other's request stored, and the two rows
	/// stay a comparison of handlers rather than of cache states.
	async fn sweep(services: &Services, base: &str) -> Result<String> {
		wait_until_ready(services, base).await?;
		register(services, "mediafederation", TOKEN).await?;

		let dest = services.globals.server_name();
		let cases = CORPUS
			.iter()
			.flat_map(|source| asks().map(move |ask| (source, ask)));

		cases
			.stream()
			.map(Ok)
			.try_fold(String::new(), async |mut report, (source, ask)| {
				let peer = over_federation(services, base, dest, source, &ask).await?;
				let client = over_client(services, base, dest, source, &ask).await?;

				report.push_str(&peer);
				report.push('\n');
				report.push_str(&client);
				report.push('\n');

				Ok(report)
			})
			.await
	}

	/// Uploads one source and asks ourselves for its thumbnail over federation.
	///
	/// The resize method is pinned rather than taken from the ask, because
	/// `asks()` pins that axis too; should it ever vary, this has to follow or
	/// the paired rows stop being comparable.
	async fn over_federation(
		services: &Services,
		base: &str,
		server_name: &ServerName,
		source: &Source,
		ask: &Ask,
	) -> Result<String> {
		let media_id = upload(services, base, TOKEN, source, None).await?;
		let request = Request {
			media_id,
			method: Some(Method::Scale),
			width: UInt::from(ask.width),
			height: UInt::from(ask.height),
			animated: ask.animated,
			timeout_ms: TIMEOUT,
		};

		let response = services
			.federation
			.execute(server_name, request)
			.await;

		Ok(row("fed", source, ask, &answered(response)))
	}

	/// Uploads one source and asks for its thumbnail over the client endpoint.
	///
	/// The client arm reuses the shared HTTP helper the other media binaries
	/// drive, where the federation arm has to build a ruma request of its own.
	async fn over_client(
		services: &Services,
		base: &str,
		server_name: &ServerName,
		source: &Source,
		ask: &Ask,
	) -> Result<String> {
		let media_id = upload(services, base, TOKEN, source, None).await?;
		let url = format!("{base}/_matrix/client/v1/media/thumbnail/{server_name}/{media_id}");
		let answer = thumbnail(services, &url, Some(TOKEN), None, ask).await?;

		// "v1" must match media_baseline.rs, so the two snapshots read together
		Ok(row("v1", source, ask, &answer))
	}

	/// Renders a federation response as the answer a peer observed.
	///
	/// A redirect is recorded by its target rather than followed, since what
	/// the endpoint chose to answer with is the thing under test.
	fn answered(result: Result<Response>) -> Answer {
		result.map_or_else(failed, |response| match response.content {
			| FileOrLocation::Location(url) => Answer {
				status: 307,
				content_type: None,
				disposition: None,
				body: format!("redirect {url}"),
			},
			| FileOrLocation::File(content) => Answer {
				status: 200,
				content_type: content.content_type.map(Into::into),
				disposition: content
					.content_disposition
					.as_ref()
					.map(ToString::to_string),
				body: describe(&content.file),
			},
		})
	}

	/// An answer standing for a request that never produced one.
	///
	/// The status is zero because no response carried a status, which keeps it
	/// distinct from every real one rather than borrowing a plausible code.
	fn failed(reason: impl Display) -> Answer {
		Answer {
			status: 0,
			content_type: None,
			disposition: None,
			body: format!("failed: {reason}"),
		}
	}
}
