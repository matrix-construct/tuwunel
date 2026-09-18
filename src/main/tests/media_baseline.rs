#![cfg(all(test, feature = "media_thumbnail"))]

//! Golden baseline for what the thumbnail endpoints answer.
//!
//! Sweeps a corpus of source pictures against every `Dim::normalized` bucket
//! and every state of the MSC2705 `animated` parameter, over both client
//! surfaces, and pins what a client observes as a snapshot. Both are swept
//! because they reach one service through separate handlers, so either can
//! regress alone. The same boot reads the default media policy on every
//! client media route, download and thumbnail alike, since a header the
//! snapshot does not carry needs a check of its own.
//!
//! Regenerate deliberately, never to make a red run green:
//! `INSTA_FORCE_UPDATE=1 cargo +nightly test --test media_baseline`.

mod media;
#[path = "media/policy.rs"]
mod policy;

// clippy's tests_outside_test_module does not see the compound cfg above as a
// test module, so the wrapper is load-bearing rather than ceremony
#[cfg(test)]
mod tests {
	use std::net::TcpListener;

	use futures::{StreamExt, TryStreamExt, future::join};
	use insta::{assert_snapshot, with_settings};
	use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
	use tuwunel_core::{Result, utils::stream::IterStream};
	use tuwunel_service::Services;

	use super::{
		media::{
			Ask, CORPUS, DatabasePath, Source, asks, register, row, thumbnail, upload,
			wait_until_ready,
		},
		policy::{TOKEN, check},
	};

	/// The policy every client media answer carries, spelled as the wire does.
	///
	/// Written out rather than joined from the router's own constant, so the
	/// check is of what a client receives and not of what the server meant.
	const POLICY: &str = concat!(
		"sandbox;default-src 'none';script-src 'none';font-src 'none';",
		"form-action 'none';base-uri 'none';style-src 'unsafe-inline'",
	);

	/// One of the two client thumbnail surfaces.
	///
	/// The legacy arm is reachable only where an operator sets
	/// `allow_legacy_media`, which defaults off, so this test turns it on to
	/// sweep a surface most deployments no longer expose.
	#[derive(Clone, Copy)]
	enum Surface {
		/// The authenticated endpoint under `/_matrix/client/v1`.
		Authenticated,

		/// The unauthenticated endpoint under `/_matrix/media/v3`.
		Legacy,
	}

	impl Surface {
		/// The path segment this surface answers on.
		///
		/// Only the segment differs between the two, so the caller builds one
		/// URL rather than one per arm.
		const fn path(self) -> &'static str {
			match self {
				| Self::Authenticated => "_matrix/client/v1/media/thumbnail",
				| Self::Legacy => "_matrix/media/v3/thumbnail",
			}
		}

		/// The name this surface is reported under.
		///
		/// The report is read as a table, so the two names are kept short and
		/// of similar width rather than spelled out.
		const fn name(self) -> &'static str {
			match self {
				| Self::Authenticated => "v1",
				| Self::Legacy => "legacy",
			}
		}
	}

	/// Sweeps the corpus over both client thumbnail surfaces and pins the
	/// answers.
	///
	/// A row that moves is a change in what some client is served, whether or
	/// not any client asked for it, and a row that moves for `animated` absent
	/// is one nothing asked for at all, since absent is the only state clients
	/// send today. The snapshot is therefore the change report for any edit to
	/// the thumbnail path. The policy read that follows it covers the header
	/// the snapshot leaves out, on the download routes as well.
	#[test]
	fn thumbnail_surface_baseline() -> Result {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();

		let db_path = DatabasePath(Args::test_database_path("media-baseline"));

		let args = [
			format!("database_path={:?}", db_path.0),
			"address=[\"127.0.0.1\"]".to_owned(),
			format!("port={port}"),
			"listening=true".to_owned(),
			"allow_legacy_media=true".to_owned(),
			"log=\"error\"".to_owned(),
		]
		.into_iter()
		.fold(Args::default_test(&["fresh", "cleanup"]), Args::with_option);

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;

		assert!(!server.server.config.media_deny_framing);
		assert!(!server.server.config.media_deny_inline_styles);

		let result = runtime.block_on(async {
			let services = async_start(&server).await?;
			let base = format!("http://127.0.0.1:{port}");

			drop(listener);

			let exercise = async {
				let report = sweep(&services, &base).await;
				let policed = check(&services, &base, POLICY).await;
				let shutdown = server.server.shutdown();

				// the shutdown makes the report evidence and the sweep's account
				// carries the policy read, so each failure outranks the next
				shutdown
					.and(report)
					.and_then(|report| policed.map(|()| report))
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
			description => "Thumbnail surface baseline",
			snapshot_suffix => "thumbnail_surface",
			omit_expression => true,
		}, {
			assert_snapshot!(report);
		});

		Ok(())
	}

	/// Runs every case and renders the report.
	///
	/// The cases run one at a time rather than fanned out, so the snapshot
	/// records the server's behavior and never the order a scheduler happened
	/// to interleave requests in.
	async fn sweep(services: &Services, base: &str) -> Result<String> {
		wait_until_ready(services, base).await?;
		register(services, "mediabaseline", TOKEN).await?;

		let server_name = services.globals.server_name();
		let surfaces = [Surface::Authenticated, Surface::Legacy];
		let cases = surfaces.into_iter().flat_map(|surface| {
			CORPUS
				.iter()
				.flat_map(move |source| asks().map(move |ask| (surface, source, ask)))
		});

		cases
			.stream()
			.then(async |(surface, source, ask)| {
				case(services, base, server_name.as_str(), surface, source, &ask).await
			})
			.map_ok(|mut line| {
				line.push('\n');

				line
			})
			.try_collect()
			.await
	}

	/// Uploads one source and requests one thumbnail of it over one surface.
	///
	/// The upload is per case rather than per source, because the variant a
	/// request stores is visible to every later request for the same picture at
	/// the same size, which would otherwise make a case's answer depend on the
	/// cases that ran before it. The surface decides both the path and whether
	/// the request carries a token, since the legacy endpoint takes none.
	async fn case(
		services: &Services,
		base: &str,
		server_name: &str,
		surface: Surface,
		source: &Source,
		ask: &Ask,
	) -> Result<String> {
		let media_id = upload(services, base, TOKEN, source, None).await?;

		let token = matches!(surface, Surface::Authenticated).then_some(TOKEN);
		let url = format!("{base}/{}/{server_name}/{media_id}", surface.path());
		let answer = thumbnail(services, &url, token, None, ask).await?;

		Ok(row(surface.name(), source, ask, &answer))
	}
}
