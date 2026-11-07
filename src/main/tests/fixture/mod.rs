//! Server-boot fixture shared by the integration tests that opt into it.
//!
//! It stands apart from the client harness so that only a binary declaring it
//! compiles it; the harness's other users stay free of its dead code. A
//! binary declaring it also declares the client harness, whose readiness
//! probe the boot awaits.

use std::{fs::remove_dir_all, net::TcpListener, path::Path};

use futures::{FutureExt, TryFutureExt, future::join};
use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
use tuwunel_core::Result;
use tuwunel_service::Services;

use super::client::wait_until_ready;

/// Boot an isolated server on an ephemeral port and run one exercise on it.
///
/// The database is named for the test and removed afterwards, and `options`
/// apply after, so they can override, the settings every test shares. The
/// exercise starts once the listener answers, and the server shuts down
/// whatever it returns.
pub(crate) fn boot<'a, Options, Exercise>(
	name: &str,
	options: Options,
	exercise: Exercise,
) -> Result
where
	Options: IntoIterator<Item = &'a str>,
	Exercise: AsyncFnOnce(&Services, &str) -> Result,
{
	let database = Args::test_database_path(name);
	let outcome = boot_at(&database, &["fresh", "cleanup"], options, exercise);

	remove_dir_all(database).ok();

	outcome
}

/// Boot a server on the database at a given path and run one exercise on it.
///
/// Only the named test harnesses apply and the database is left in place, so a
/// test reopening one database across boots, one process each, names neither
/// `fresh` nor `cleanup` after its first. Otherwise this boots as [`boot`] does.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) fn boot_at<'a, Options, Exercise>(
	database: &Path,
	harnesses: &[&str],
	options: Options,
	exercise: Exercise,
) -> Result
where
	Options: IntoIterator<Item = &'a str>,
	Exercise: AsyncFnOnce(&Services, &str) -> Result,
{
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let address = listener.local_addr()?;

	let args = Args::default_test(harnesses)
		.with_database_path(database)
		.with_option(format!("http.listen.address=[\"{}\"]", address.ip()))
		.with_option(format!("http.listen.port={}", address.port()))
		.with_option("listening=true");

	let args = options.into_iter().fold(args, Args::with_option);

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;
	let outcome = runtime.block_on(async {
		let services = async_start(&server).await?;
		let base = format!("http://{address}");

		drop(listener);

		let session = async {
			let outcome = wait_until_ready(&services, &base)
				.and_then(|()| exercise(&services, &base))
				.boxed_local() // size firewall
				.await;

			let shutdown = server.server.shutdown();

			outcome.and(shutdown)
		};

		let (running, outcome) = join(async_run(&server), session).await;

		drop(services);

		async_stop(&server)
			.await
			.and(running)
			.and(outcome)
	});

	drop(runtime);

	outcome
}
