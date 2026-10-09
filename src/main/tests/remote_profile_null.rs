#![cfg(test)]

use std::{
	env::{current_exe, var},
	fs::remove_dir_all,
	net::TcpListener,
	path::{Path, PathBuf},
	process::Command,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
};

use axum::{Json, Router, extract::State, routing::get};
use axum_server::{from_tcp_rustls, tls_rustls::RustlsConfig};
use futures::future::join;
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{Result, ruma::UserId};
use tuwunel_service::Services;

use self::client::wait_until_ready;

#[expect(
	dead_code,
	reason = "only the readiness probe is needed from the shared harness"
)]
mod client;

const PHASE: &str = "PROFILE_NULL_TEST_PHASE";
const DATABASE: &str = "PROFILE_NULL_TEST_DATABASE";
const PEER: &str = "PROFILE_NULL_TEST_PEER";

/// Legacy null profile rows remain refreshable across a process restart.
///
/// Full and field client reads publish valid replacements fetched from a peer.
#[test]
fn remote_null_profiles_survive_refresh_and_reopen() -> Result {
	if let Ok(phase) = var(PHASE) {
		let database = PathBuf::from(var(DATABASE).expect("child database is configured"));
		let peer = var(PEER).expect("child peer is configured");

		return boot(&database, &peer, phase == "seed");
	}

	let database = Args::test_database_path("remote-profile-null");
	let reservation = TcpListener::bind(("127.0.0.1", 0))?;
	let peer = reservation.local_addr()?.to_string();

	drop(reservation);

	let outcome = ["seed", "reopen"]
		.into_iter()
		.try_for_each(|phase| {
			let status = Command::new(current_exe()?)
				.env(PHASE, phase)
				.env(DATABASE, &database)
				.env(PEER, &peer)
				.status()?;

			assert!(status.success(), "{phase} process failed: {status}");

			Ok(())
		});

	remove_dir_all(database).ok();

	outcome
}

fn boot(database: &Path, peer: &str, seed: bool) -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let address = listener.local_addr()?;
	let tests: &[&str] = if seed { &["fresh"] } else { &[] };
	let args = Args::default_test(tests)
		.with_database_path(database)
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={}", address.port()))
		.with_option("listening=true")
		.with_option("allow_invalid_tls_certificates=true")
		.with_option("ip_range_denylist=[]")
		.with_option("federation_loopback=true");

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://{address}");

		drop(listener);

		let session = async {
			let outcome = exercise(&services, &base, peer, seed).await;
			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (running, outcome) = join(async_run(&server), session).await;

		drop(services);

		async_stop(&server)
			.await
			.and(running)
			.and(outcome)
	})
}

async fn exercise(services: &Services, base: &str, peer: &str, seed: bool) -> Result {
	wait_until_ready(services, base).await?;

	let user = UserId::parse(format!("@remote:{peer}"))?;
	let rows = &services.db["useridprofilekey_value"];

	if seed {
		services.users.create(&user, None, None).await?;
		rows.put_raw((&user, "displayname"), br#""Before""#);
		rows.put_raw((&user, "avatar_url"), b"null");
	}

	let avatar: Value = services
		.profile
		.profile_key(&user, &"avatar_url".into())
		.await?;

	assert_eq!(avatar, Value::Null, "legacy null must survive reopening");

	let listener = TcpListener::bind(peer)?;

	listener.set_nonblocking(true)?;

	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	let config = RustlsConfig::from_pem_file(
		manifest.join("../../nix/pkgs/complement/certificate.crt"),
		manifest.join("../../nix/pkgs/complement/private_key.key"),
	)
	.await?;

	let requests = Arc::new(AtomicUsize::new(0));
	let app = Router::new()
		.route("/_matrix/federation/v1/query/profile", get(answer))
		.with_state(requests.clone());

	let peer_server = from_tcp_rustls(listener, config)?.serve(app.into_make_service());
	let mut tasks = JoinSet::new(); // The set aborts the peer when the exercise exits.

	tasks.spawn_on(peer_server, services.server.runtime());

	let path = format!("profile/{user}");
	let before = services.globals.current_count();

	for expected in 1..=2 {
		services
			.profile
			.fetch_remote_profile(&user)
			.await?;

		assert_eq!(services.profile.displayname(&user).await?, "Before");
		assert_eq!(requests.load(Ordering::SeqCst), expected);
		assert_eq!(services.globals.current_count(), before);
	}

	if seed {
		return Ok(());
	}

	let profile = read(services, base, &path).await?;

	assert_eq!(profile["displayname"], "After");
	assert_eq!(profile["avatar_url"], "mxc://remote.example/new");
	assert_eq!(requests.load(Ordering::SeqCst), 3);
	assert!(services.globals.current_count() > before);

	let after = services.globals.current_count();
	let field = read(services, base, &format!("{path}/avatar_url")).await?;

	assert_eq!(field["avatar_url"], "mxc://remote.example/new");
	assert_eq!(requests.load(Ordering::SeqCst), 4);
	assert_eq!(services.globals.current_count(), after);

	Ok(())
}

async fn answer(State(requests): State<Arc<AtomicUsize>>) -> Json<Value> {
	let request = requests.fetch_add(1, Ordering::SeqCst);
	let profile = match request {
		| 0 | 1 => json!({"displayname": "Before", "avatar_url": null}),
		| _ => json!({"displayname": "After", "avatar_url": "mxc://remote.example/new"}),
	};

	Json(profile)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn read(services: &Services, base: &str, path: &str) -> Result<Value> {
	let response = services
		.client
		.clients
		.default
		.get(format!("{base}/_matrix/client/v3/{path}"))
		.send()
		.await?;

	let status = response.status();
	let body = response.json().await?;

	assert!(status.is_success(), "{path}: {status}: {body}");

	Ok(body)
}
