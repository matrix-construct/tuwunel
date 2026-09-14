#![cfg(all(test, feature = "media_thumbnail"))]

//! Media behavior an appservice sees, which is what bridges depend on.
//!
//! Bridges are among the heaviest media clients a homeserver has, and they
//! reach the media endpoints with an appservice token rather than a user one,
//! usually masquerading as a ghost. Nothing else in the suite covers that
//! combination, so this pins that the appservice path answers exactly what the
//! user path answers and that the upload surface a bridge drives keeps
//! working, the namespace check included.
//!
//! Parity is asserted rather than snapshotted, because a second snapshot would
//! drift against the baseline silently where a row-for-row comparison fails
//! loudly and names the case.

mod media;

// clippy's tests_outside_test_module does not see the compound cfg above as a
// test module, so the wrapper is load-bearing rather than ceremony
#[cfg(test)]
mod tests {
	use std::net::TcpListener;

	use futures::future::join;
	use serde_json::Value;
	use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
	use tuwunel_core::{
		Err, Result, err,
		ruma::api::appservice::{Namespace, Namespaces, Registration, RegistrationInit},
	};
	use tuwunel_service::Services;

	use super::media::{
		Ask, CORPUS, DatabasePath, Source, asks, register, row, thumbnail, upload,
		wait_until_ready,
	};

	const USER_TOKEN: &str = "media-appservice-user-access-token";

	const AS_TOKEN: &str = "media-appservice-bridge-access-token";

	/// The surface column, empty because both callers share one.
	///
	/// A differing label would make every comparison diverge on the label
	/// alone, so the field is blanked rather than named.
	const NO_SURFACE: &str = "";

	/// Corpus sources the parity sweep covers.
	///
	/// The whole corpus would double the baseline's work to prove the same
	/// thing twice, so this keeps the shapes where an animated answer is
	/// possible plus one ordinary still as a control.
	const PARITY_SOURCES: &[&str] = &[
		"still_100x100.png",
		"still_100x100.webp",
		"anim_100x100.gif",
		"anim_100x100.webp",
		"anim_100x100.apng",
	];

	/// Who a request is made as.
	///
	/// The two differ in more than a token: a bridge also names the ghost it is
	/// acting for, and the pair travels together through the whole sweep, so it
	/// is carried as one closed choice rather than a token beside a bare
	/// `None`.
	#[derive(Clone, Copy)]
	enum Caller<'a> {
		/// An ordinary local user, authenticating with its own token.
		User,

		/// An appservice acting for a ghost inside its namespace.
		Bridge(&'a str),
	}

	/// Drives the media endpoints as an appservice and pins what bridges depend
	/// on.
	///
	/// Three behaviors are covered: that an appservice may write media as
	/// itself and as a ghost it claims but not as a user outside its
	/// namespace, that the two-step upload round-trips, and that thumbnails
	/// answer identically under both token kinds.
	#[test]
	fn appservice_media_matches_user_media() -> Result {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();

		let db_path = DatabasePath(Args::test_database_path("media-appservice"));

		let args = [
			format!("database_path={:?}", db_path.0),
			"address=[\"127.0.0.1\"]".to_owned(),
			format!("port={port}"),
			"listening=true".to_owned(),
			"log=\"error\"".to_owned(),
		]
		.into_iter()
		.fold(Args::default_test(&["fresh", "cleanup"]), Args::with_option);

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;
		let result = runtime.block_on(async {
			let services = async_start(&server).await?;
			let base = format!("http://127.0.0.1:{port}");

			drop(listener);

			let run = async {
				let outcome = drive(&services, &base).await;
				let shutdown = server.server.shutdown();

				outcome.and(shutdown)
			};

			let (run_result, outcome) = join(async_run(&server), run).await;

			drop(services);
			async_stop(&server).await?;
			run_result?;

			outcome
		});

		drop(runtime);

		result
	}

	/// Registers the accounts the sweep needs and runs each behavior in turn.
	///
	/// The namespace and upload checks run before the parity sweep, since a
	/// failure there would make every parity row fail for the same reason.
	async fn drive(services: &Services, base: &str) -> Result {
		wait_until_ready(services, base).await?;
		register(services, "mediaappservice", USER_TOKEN).await?;
		register_bridge(services).await?;

		let server_name = services.globals.server_name();
		let ghost = format!("@_bridge_ghost:{server_name}");

		uploads_honor_the_namespace(services, base, &ghost).await?;
		async_upload_round_trips(services, base).await?;
		parity(services, base, server_name.as_str(), &ghost).await
	}

	/// Registers an appservice whose namespace claims the ghosts it will act
	/// as.
	///
	/// It carries no URL, because nothing here sends it a transaction; only its
	/// token and namespace are under test.
	async fn register_bridge(services: &Services) -> Result {
		let namespaces = Namespaces {
			users: vec![Namespace::new(true, "@_bridge_.*".to_owned())],
			..Default::default()
		};

		let registration: Registration = RegistrationInit {
			id: "mediabridge".to_owned(),
			url: None,
			as_token: AS_TOKEN.to_owned(),
			hs_token: "media-appservice-bridge-hs-token".to_owned(),
			sender_localpart: "_bridge".to_owned(),
			namespaces,
			rate_limited: None,
			protocols: None,
		}
		.into();

		services
			.appservice
			.register_appservice(registration)
			.await
	}

	/// Uploads as the appservice itself and as a ghost it claims, and no
	/// further.
	///
	/// The out-of-namespace case is the one worth pinning: a bridge must not be
	/// able to write media under a user it does not own.
	async fn uploads_honor_the_namespace(services: &Services, base: &str, ghost: &str) -> Result {
		let source = pick("still_100x100.png")?;

		upload(services, base, AS_TOKEN, source, None).await?;
		upload(services, base, AS_TOKEN, source, Some(ghost)).await?;

		let server_name = services.globals.server_name();
		let outsider = format!("@notthebridge:{server_name}");

		let response = services
			.client
			.clients
			.default
			.post(format!("{base}/_matrix/media/v3/upload"))
			.bearer_auth(AS_TOKEN)
			.query(&[("user_id", outsider.as_str())])
			.header("content-type", source.content_type)
			.body(source.bytes)
			.send()
			.await?;

		let status = response.status().as_u16();
		let body: Value = response.json().await.unwrap_or_default();
		let errcode = body.get("errcode").and_then(Value::as_str);

		// the status alone would pass for any 400, so the errcode is what shows the
		// namespace check is the thing that refused
		match (status, errcode) {
			| (400, Some("M_EXCLUSIVE")) => Ok(()),
			| _ => Err!("an out-of-namespace masquerade answered {status} {errcode:?}"),
		}
	}

	/// Reserves a media id, fills it, and reads it back.
	///
	/// This is the two-step upload (MSC2246) a bridge uses when it must know
	/// the mxc before it has the bytes, so the round trip is asserted on the
	/// content rather than on the status alone.
	async fn async_upload_round_trips(services: &Services, base: &str) -> Result {
		let source = pick("anim_100x100.gif")?;
		let client = &services.client.clients.default;

		let created: Value = client
			.post(format!("{base}/_matrix/media/v1/create"))
			.bearer_auth(AS_TOKEN)
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		let uri = created
			.get("content_uri")
			.and_then(Value::as_str)
			.ok_or_else(|| err!("create answered without a content_uri: {created}"))?;

		let (server_name, media_id) = uri
			.trim_start_matches("mxc://")
			.split_once('/')
			.ok_or_else(|| err!("content_uri is not an mxc: {uri}"))?;

		client
			.put(format!("{base}/_matrix/media/v3/upload/{server_name}/{media_id}"))
			.bearer_auth(AS_TOKEN)
			.header("content-type", source.content_type)
			.body(source.bytes)
			.send()
			.await?
			.error_for_status()?;

		let downloaded = client
			.get(format!("{base}/_matrix/client/v1/media/download/{server_name}/{media_id}"))
			.bearer_auth(AS_TOKEN)
			.send()
			.await?
			.error_for_status()?
			.bytes()
			.await?;

		match downloaded.as_ref() == source.bytes {
			| true => Ok(()),
			| false => Err!(
				"an async upload round-tripped {} bytes, not the {} uploaded",
				downloaded.len(),
				source.bytes.len()
			),
		}
	}

	/// Sweeps the same requests under both callers and requires agreement.
	///
	/// Divergences are collected rather than raised at the first, so one run
	/// reports the whole shape of a bridge-only regression. A transport failure
	/// still aborts, since that is harness breakage rather than a finding.
	async fn parity(services: &Services, base: &str, server_name: &str, ghost: &str) -> Result {
		let mut divergences = Vec::new();

		for name in PARITY_SOURCES {
			let source = pick(name)?;

			for ask in asks() {
				let user = case(services, base, server_name, Caller::User, source, &ask).await?;

				let bridge =
					case(services, base, server_name, Caller::Bridge(ghost), source, &ask)
						.await?;

				if user != bridge {
					divergences.push(format!("  user  : {user}\n  bridge: {bridge}"));
				}
			}
		}

		match divergences.is_empty() {
			| true => Ok(()),
			| false => Err!(
				"the appservice path answered differently from the user path in {} case(s):\n{}",
				divergences.len(),
				divergences.join("\n")
			),
		}
	}

	/// Uploads one source as one caller and renders the thumbnail answer.
	///
	/// A bridge masquerades on the read as well as the upload, which is what a
	/// real one does, and both callers upload their own copy so the answer each
	/// sees is generated for it rather than read from a variant the other
	/// stored. Only the authenticated surface is swept, since the legacy one
	/// takes no token and so cannot tell the two callers apart.
	async fn case(
		services: &Services,
		base: &str,
		server_name: &str,
		caller: Caller<'_>,
		source: &Source,
		ask: &Ask,
	) -> Result<String> {
		let token = caller.token();
		let ghost = caller.masquerade();
		let media_id = upload(services, base, token, source, ghost).await?;

		let url = format!("{base}/_matrix/client/v1/media/thumbnail/{server_name}/{media_id}");
		let answer = thumbnail(services, &url, Some(token), ghost, ask).await?;

		Ok(row(NO_SURFACE, source, ask, &answer))
	}

	/// The corpus entry by name, which the parity list refers to.
	///
	/// A name the corpus does not carry is a mistake in this file rather than a
	/// server behavior, so it fails the test loudly instead of being skipped.
	fn pick(name: &str) -> Result<&'static Source> {
		CORPUS
			.iter()
			.find(|source| source.name == name)
			.ok_or_else(|| err!("the corpus carries no {name}"))
	}

	// #[implement] does not declare a named lifetime for the free function it
	// generates, which `masquerade`'s return type needs, so this stays a block
	impl<'a> Caller<'a> {
		/// The access token this caller authenticates with.
		///
		/// A bridge presents its own token even when acting for a ghost,
		/// because a ghost never has one.
		const fn token(self) -> &'static str {
			match self {
				| Self::User => USER_TOKEN,
				| Self::Bridge(_) => AS_TOKEN,
			}
		}

		/// The user an upload is attributed to, when it is not the token's own.
		///
		/// Only a bridge names one; a user's uploads are already its own.
		const fn masquerade(self) -> Option<&'a str> {
			match self {
				| Self::User => None,
				| Self::Bridge(ghost) => Some(ghost),
			}
		}
	}
}
