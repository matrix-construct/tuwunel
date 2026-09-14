#![cfg(test)]
#![cfg(feature = "ldap")]

use std::{
	io::ErrorKind,
	net::TcpListener,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	thread,
	time::Duration,
};

use tuwunel::{Args, Runtime, Server};
use tuwunel_core::{
	Err, Result,
	ruma::{
		UserId,
		api::client::uiaa::{AuthData, MatrixUserIdentifier, Password, UiaaInfo, UserIdentifier},
	},
};
use tuwunel_service::Services;

/// A password-origin account must not reach LDAP during UIA reauthentication,
/// even with LDAP enabled. Otherwise a wrong password triggers a directory-wide
/// search against the configured server (matrix-construct/tuwunel#255).
#[test]
fn uiaa_password_account_skips_ldap() -> Result {
	// A throwaway listener stands in for the LDAP server. Any connection it
	// accepts is recorded; a non-LDAP account must leave the count at zero.
	let listener = TcpListener::bind("127.0.0.1:0")?;
	let port = listener.local_addr()?.port();
	listener.set_nonblocking(true)?;

	let hits = Arc::new(AtomicUsize::new(0));
	let stop = Arc::new(AtomicBool::new(false));
	let probe = thread::spawn({
		let hits = hits.clone();
		let stop = stop.clone();
		move || {
			while !stop.load(Ordering::Relaxed) {
				match listener.accept() {
					| Ok(_) => {
						hits.fetch_add(1, Ordering::Relaxed);
					},
					| Err(e) if e.kind() == ErrorKind::WouldBlock => {
						thread::sleep(Duration::from_millis(10));
					},
					| Err(_) => break,
				}
			}
		}
	});

	let db_path = Args::test_database_path("uiaa-ldap");

	let args = Args::default_test(&["fresh", "cleanup"])
		.with_database_path(&db_path)
		.with_maintenance()
		.with_option("ldap.enable=true")
		.with_option(format!("ldap.uri=\"ldap://127.0.0.1:{port}\""));

	let runtime = Runtime::new(Some(&args))?;
	let server = Server::new(Some(&args), Some(&runtime))?;

	let result: Result = runtime.block_on(async {
		let services = tuwunel::async_start(&server).await?;

		let outcome = reauth(&services).await;

		server.server.shutdown()?;
		drop(services);

		tuwunel::async_run(&server).await?;
		tuwunel::async_stop(&server).await?;

		outcome
	});

	drop(runtime);

	stop.store(true, Ordering::Relaxed);
	probe.join().ok();

	// `cleanup` only fires on a clean DB close, which dangling refs can prevent.
	std::fs::remove_dir_all(&db_path).ok();

	result?;

	let hits = hits.load(Ordering::Relaxed);
	if hits != 0 {
		return Err!("LDAP was contacted {hits} time(s) for a password-origin account");
	}

	Ok(())
}

async fn reauth(services: &Arc<Services>) -> Result {
	let user_id = UserId::parse_with_server_name("alice", services.globals.server_name())?;

	services
		.users
		.create(&user_id, Some("correct-horse"), None)
		.await?;

	let auth = AuthData::Password(Password::new(
		UserIdentifier::Matrix(MatrixUserIdentifier::new(user_id.localpart().to_owned())),
		"wrong-password".to_owned(),
	));

	let (worked, _) = services
		.uiaa
		.try_auth(&user_id, "ALICEDEVICE".into(), &auth, &UiaaInfo::default())
		.await?;

	if worked {
		return Err!("a wrong password must not satisfy UIA");
	}

	Ok(())
}
