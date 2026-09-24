//! Boots the token-expiry passes against a seeded origin column.
//!
//! The origin column is outside this server's catalog and nothing here can
//! create it, so the parent process writes it with the storage engine between
//! child boots, alongside a token row in one of the shapes a migrated database
//! holds and with the pre-stamped markers cleared to match. Each boot is a child
//! process because a stopped server keeps the database lock until it exits.
//!
//! Without an OIDC provider the adoption must leave the token alone and its
//! marker unstamped, because the client's refresh would fail at discovery and
//! never sign the session out, and finish at once on a column with nothing to
//! adopt; with a provider it must adopt, refuse the token with the sign-out
//! signal, and answer the refresh with the one OAuth error the client acts on.
//! A row the ungated release stamped must be restored and handed back to the
//! adoption at once, and only while no provider could refresh it.

#![cfg(test)]

use std::{
	env::{current_exe, var},
	fs::{read_to_string, remove_dir_all, remove_file, write},
	net::TcpListener,
	path::{Path, PathBuf},
	process::Command,
};

use futures::{
	TryFutureExt,
	future::{join, try_join},
};
use reqwest::StatusCode;
use rust_rocksdb::{DB, Error as EngineError, Options};
use serde_json::Value;
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::{
	Err, Error, Result, err,
	ruma::{OwnedDeviceId, OwnedUserId},
	utils::result::NotFound,
};
use tuwunel_database::serialize_key;
use tuwunel_service::Services;

use self::client::{register, wait_until_ready};

#[expect(
	dead_code,
	reason = "the shared client harness exposes helpers used by sibling integration tests"
)]
mod client;

const CHILD_DATABASE_ENV: &str = "TOKEN_EXPIRY_TEST_DATABASE";
const CHILD_PHASE_ENV: &str = "TOKEN_EXPIRY_TEST_PHASE";
const FOREIGN_TOKEN: &str = "token-expiry-adoption-foreign-access-token";
const NATIVE_TOKEN: &str = "token-expiry-adoption-native-access-token";
const ADOPT_MARKER: &[u8] = b"adopt_foreign_token_expiry";
const RESTORE_MARKER: &[u8] = b"restore_foreign_token_expiry";
const ORIGIN_COLUMN: &str = "userdeviceid_tokenexpires";
const PAST_EXPIRY_SECS: u64 = 1_600_000_000;

type Session = (OwnedUserId, OwnedDeviceId);

struct ScratchDatabase(PathBuf);

#[derive(Clone, Copy)]
enum Phase {
	Register,
	WithoutProvider,
	WithProvider,
}

/// The shape the seeded session's token row is left in.
///
/// It also decides which pre-stamped markers are cleared, since each shape is
/// what one earlier release leaves behind.
#[derive(Clone, Copy)]
enum Seed {
	/// The origin's two-field row, as a release before either pass left it.
	Foreign,

	/// A row this server wrote, as a release before either pass left it.
	Native,

	/// The row the ungated release stamped, with its marker still set.
	Adopted,
}

#[test]
fn foreign_expiry_adoption_waits_for_a_provider() -> Result {
	if let Ok(phase) = var(CHILD_PHASE_ENV) {
		let database = PathBuf::from(
			var(CHILD_DATABASE_ENV).expect("token expiry child database is configured"),
		);

		return match phase.as_str() {
			| "register" => boot(&database, Phase::Register, register_phase),
			| "without-provider" => boot(&database, Phase::WithoutProvider, unadopted_phase),
			| "settled" => boot(&database, Phase::WithoutProvider, finished_phase),
			| "restored" => boot(&database, Phase::WithoutProvider, restored_phase),
			| "kept" => boot(&database, Phase::WithProvider, kept_phase),
			| "with-provider" => boot(&database, Phase::WithProvider, adopted_phase),
			| _ => Err!("unknown token expiry child phase: {phase}"),
		};
	}

	let phases = ["without-provider", "without-provider", "with-provider"];

	run_case("token-expiry-adoption", Seed::Foreign, &phases)?;
	run_case("token-expiry-adoption-settled", Seed::Native, &["settled"])?;
	run_case("token-expiry-adoption-stamped", Seed::Adopted, &["restored", "with-provider"])?;
	run_case("token-expiry-adoption-kept", Seed::Adopted, &["kept", "restored"])
}

fn run_case(name: &str, seed: Seed, phases: &[&str]) -> Result {
	let database = ScratchDatabase(Args::test_database_path(name));

	run_child(database.path(), "register")?;
	seed_origin_column(database.path(), &read_session(database.path())?, seed)?;

	phases
		.iter()
		.try_for_each(|phase| run_child(database.path(), phase))
}

impl ScratchDatabase {
	#[inline]
	fn path(&self) -> &Path { &self.0 }
}

impl Drop for ScratchDatabase {
	fn drop(&mut self) {
		remove_dir_all(&self.0).ok();
		remove_file(sidecar(&self.0)).ok();
	}
}

/// Boots the server on the fixture database and runs one phase against it.
///
/// The port is bound first so the phase can be told where the server will
/// listen, and released just before the server binds it. The phase's outcome
/// takes precedence over the shutdown's, and the stop runs after both so a
/// failed phase still leaves no server behind.
fn boot<F>(database: &Path, phase: Phase, exercise: F) -> Result
where
	F: AsyncFnOnce(&Services, &str, &Path) -> Result,
{
	let phase_error = |error: Error| err!("token expiry adoption boot failed: {error}");
	let listener =
		TcpListener::bind(("127.0.0.1", 0)).map_err(|error| phase_error(error.into()))?;

	let port = listener
		.local_addr()
		.map_err(|error| phase_error(error.into()))?
		.port();

	let args = phase_args(database, port, phase);
	let runtime = Runtime::new(Some(&args)).map_err(&phase_error)?;
	let server = Server::new(Some(&args), Some(&runtime)).map_err(&phase_error)?;
	let result = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://127.0.0.1:{port}");

		drop(listener);

		let run = async {
			let outcome = wait_until_ready(&services, &base)
				.and_then(|()| exercise(&services, &base, database))
				.await;

			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (run_result, outcome) = join(async_run(&server), run).await;

		drop(services);
		async_stop(&server).await?;
		run_result?;

		outcome
	});

	drop(server);
	drop(runtime);

	result.map_err(phase_error)
}

fn phase_args(database: &Path, port: u16, phase: Phase) -> Args {
	let (harnesses, provider): (&[&str], &[&str]) = match phase {
		| Phase::Register => (&["fresh"], &[]),
		| Phase::WithoutProvider => (&[], &[]),
		| Phase::WithProvider =>
			(&[], &["well_known.client=\"https://localhost\"", "oidc_native_auth=true"]),
	};

	let args = Args::default_test(harnesses)
		.with_database_path(database)
		.with_option("address=[\"127.0.0.1\"]")
		.with_option(format!("port={port}"))
		.with_option("listening=true")
		.with_option("log_enable=false");

	provider
		.iter()
		.copied()
		.fold(args, Args::with_option)
}

async fn register_phase(services: &Services, _: &str, database: &Path) -> Result {
	try_join(
		register(services, "origin", FOREIGN_TOKEN),
		register(services, "native", NATIVE_TOKEN),
	)
	.await?;

	let (user_id, device_id, _) = services
		.users
		.find_from_token(FOREIGN_TOKEN)
		.await?;

	write(sidecar(database), format!("{user_id}\n{device_id}"))?;

	Ok(())
}

async fn unadopted_phase(services: &Services, base: &str, _: &Path) -> Result {
	tokens_live(services, base).await?;
	assert!(
		!marker_stamped(services, ADOPT_MARKER).await?,
		"skip must leave the marker clear"
	);

	assert!(
		marker_stamped(services, RESTORE_MARKER).await?,
		"a column with nothing to restore must finish the restore"
	);

	Ok(())
}

async fn finished_phase(services: &Services, base: &str, _: &Path) -> Result {
	tokens_live(services, base).await?;
	assert!(
		marker_stamped(services, ADOPT_MARKER).await?,
		"an unadoptable column must finish the pass"
	);

	assert!(marker_stamped(services, RESTORE_MARKER).await?, "restore must stamp its marker");

	Ok(())
}

async fn restored_phase(services: &Services, base: &str, _: &Path) -> Result {
	tokens_live(services, base).await?;

	let (.., expires) = services
		.users
		.find_from_token(FOREIGN_TOKEN)
		.await?;

	assert!(expires.is_none(), "restore must put the token back into the foreign shape");
	assert!(
		!marker_stamped(services, ADOPT_MARKER).await?,
		"restore must hand the column back to the adoption"
	);

	assert!(marker_stamped(services, RESTORE_MARKER).await?, "restore must stamp its marker");

	Ok(())
}

// The stamped token is not presented, since a refusal would remove it.
async fn kept_phase(services: &Services, base: &str, _: &Path) -> Result {
	assert_eq!(whoami(services, base, NATIVE_TOKEN).await?.0, StatusCode::OK);
	assert!(
		!marker_stamped(services, RESTORE_MARKER).await?,
		"restore must wait while a provider exists"
	);

	assert!(
		marker_stamped(services, ADOPT_MARKER).await?,
		"restore must not hand back a column a provider can refresh"
	);

	Ok(())
}

async fn adopted_phase(services: &Services, base: &str, _: &Path) -> Result {
	let (status, body) = whoami(services, base, FOREIGN_TOKEN).await?;

	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["errcode"], "M_UNKNOWN_TOKEN");
	assert_eq!(body["soft_logout"], true);

	// The first refusal removed the token, so only it carries the sign-out hint.
	let (status, body) = whoami(services, base, FOREIGN_TOKEN).await?;

	assert_eq!(status, StatusCode::UNAUTHORIZED);
	assert_eq!(body["errcode"], "M_UNKNOWN_TOKEN");
	assert!(body.get("soft_logout").is_none());
	assert_eq!(whoami(services, base, NATIVE_TOKEN).await?.0, StatusCode::OK);
	assert!(marker_stamped(services, ADOPT_MARKER).await?, "adoption must stamp the marker");

	refresh_rejected(services, base).await
}

async fn refresh_rejected(services: &Services, base: &str) -> Result {
	let metadata: Value = services
		.client
		.clients
		.default
		.get(format!("{base}/_matrix/client/v1/auth_metadata"))
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	let token_endpoint = metadata["token_endpoint"]
		.as_str()
		.ok_or_else(|| err!("auth metadata names no token endpoint"))?;

	assert!(token_endpoint.ends_with("/_tuwunel/oidc/token"));

	let refresh = services
		.client
		.clients
		.default
		.post(format!("{base}/_tuwunel/oidc/token"))
		.form(&[
			("grant_type", "refresh_token"),
			("refresh_token", "origin-refresh-token"),
			("client_id", "origin-client"),
		])
		.send()
		.await?;

	assert_eq!(refresh.status(), StatusCode::BAD_REQUEST);

	let refresh: Value = refresh.json().await?;

	assert_eq!(refresh["error"], "invalid_grant");

	Ok(())
}

async fn tokens_live(services: &Services, base: &str) -> Result {
	let (foreign, native) =
		try_join(whoami(services, base, FOREIGN_TOKEN), whoami(services, base, NATIVE_TOKEN))
			.await?;

	assert_eq!(foreign.0, StatusCode::OK);
	assert_eq!(native.0, StatusCode::OK);

	Ok(())
}

fn run_child(database: &Path, phase: &str) -> Result {
	let output = Command::new(current_exe()?)
		.env(CHILD_DATABASE_ENV, database)
		.env(CHILD_PHASE_ENV, phase)
		.output()?;

	if !output.status.success() {
		let stdout = String::from_utf8_lossy(&output.stdout);
		let stderr = String::from_utf8_lossy(&output.stderr);

		return Err!(
			"token expiry {phase} child failed with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
			output.status,
		);
	}

	Ok(())
}

fn read_session(database: &Path) -> Result<Session> {
	let device = read_to_string(sidecar(database))?;
	let (user_id, device_id) = device
		.split_once('\n')
		.ok_or_else(|| err!("register phase left no device sidecar"))?;

	Ok((user_id.try_into()?, device_id.into()))
}

fn sidecar(database: &Path) -> PathBuf { database.with_extension("device") }

/// Writes what a migrated origin database holds in the origin's expiry column.
///
/// The column is created with a past expiry for the session's device, and the
/// token row is rewritten into the foreign two-field shape, left as this server
/// wrote it, or stamped as the ungated adoption left it. A database upgraded
/// from before either pass holds neither marker, while one the ungated release
/// stamped holds the adoption's, so the markers are cleared to match the seed.
fn seed_origin_column(database: &Path, (user_id, device_id): &Session, seed: Seed) -> Result {
	let opts = Options::default();
	let columns = DB::list_cf(&opts, database).map_err(storage_error)?;
	let db = DB::open_cf(&opts, database, &columns).map_err(storage_error)?;
	let device = serialize_key((user_id, device_id))?;

	db.create_cf(ORIGIN_COLUMN, &opts)
		.map_err(storage_error)?;

	let column = |name| {
		db.cf_handle(name)
			.ok_or_else(|| err!("column {name} missing from the test database"))
	};

	let owners = column("token_userdeviceid")?;
	let expiries = column(ORIGIN_COLUMN)?;
	let global = column("global")?;

	db.put_cf(&expiries, &device, PAST_EXPIRY_SECS.to_be_bytes())
		.map_err(storage_error)?;

	let row = match seed {
		| Seed::Native => None,
		| Seed::Foreign => Some(device),
		| Seed::Adopted => Some(serialize_key((user_id, device_id, Some(PAST_EXPIRY_SECS)))?),
	};

	if let Some(row) = row {
		db.put_cf(&owners, FOREIGN_TOKEN, &row)
			.map_err(storage_error)?;
	}

	db.delete_cf(&global, RESTORE_MARKER)
		.map_err(storage_error)?;

	if !matches!(seed, Seed::Adopted) {
		db.delete_cf(&global, ADOPT_MARKER)
			.map_err(storage_error)?;
	}

	db.flush_wal(true).map_err(storage_error)?;

	Ok(())
}

#[expect(
	clippy::needless_pass_by_value,
	reason = "map_err hands the engine error by value"
)]
fn storage_error(error: EngineError) -> Error { err!("storage engine: {error}") }

async fn marker_stamped(services: &Services, marker: &[u8]) -> Result<bool> {
	services.db["global"]
		.get(marker)
		.await
		.optional()
		.map(|stamped| stamped.is_some())
}

async fn whoami(services: &Services, base: &str, token: &str) -> Result<(StatusCode, Value)> {
	let response = services
		.client
		.clients
		.default
		.get(format!("{base}/_matrix/client/v3/account/whoami"))
		.bearer_auth(token)
		.send()
		.await?;

	let status = response.status();
	let body = response.json().await?;

	Ok((status, body))
}
