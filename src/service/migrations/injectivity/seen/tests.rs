use std::{cell::Cell, fmt::Debug, iter::repeat_with, sync::Arc};

use futures::future::ready;
use tuwunel_core::{
	Err, Result, Server,
	config::{Config, Figment, Sources},
	log::{LogLevelReloadHandles, Logging},
	utils::result::NotFound,
};
use tuwunel_database::{Database, Txn, TxnError};

use super::{
	Boundary, IDENTITY_LIMIT, MARKER, Outcome, Reason, SAMPLE_LIMIT, Sample, Shape, Status, run,
	stamp,
};
use crate::test_utils::fixture;

#[tokio::test]
async fn runner_contains_errors_and_honors_only_its_marker() -> Result {
	let Some(fixture) = fixture(Figment::new()).await? else {
		return Ok(());
	};

	let db = &fixture.services.db;
	let server = &fixture.services.server;
	let global = &db["global"];
	let calls = Cell::new(0);
	let repair = || {
		calls.set(calls.get() + 1);
		ready(Ok(Outcome::clean()))
	};

	assert_eq!(run(db, repair).await, Some(Outcome::clean()));
	assert_eq!(calls.get(), 1);
	global.remove(MARKER);
	calls.set(0);

	for old in [b"".as_slice(), b"declined", b"unknown"] {
		global.insert("fix_short_injectivity", old);
		assert_eq!(run(db, repair).await, Some(Outcome::clean()));
		global.remove(MARKER);
	}

	assert_eq!(calls.get(), 3);
	assert_absent(db, "global", MARKER, "new record removed between cases").await;
	global.insert(MARKER, b"unknown");
	assert_eq!(run(db, repair).await, Some(Outcome::clean()));
	assert_eq!(calls.get(), 4);
	global.insert(MARKER, Outcome::clean().encode()?);
	assert!(run(db, repair).await.is_none());
	assert_eq!(calls.get(), 4);
	global.remove(MARKER);
	let unfinished = Outcome {
		status: Status::Unfinished,
		counts: [1; 15],
		..Outcome::clean()
	};

	assert_eq!(run(db, || ready(Ok(unfinished.clone()))).await, Some(unfinished.clone()));
	assert!(run(db, repair).await.is_none());
	assert_eq!(Outcome::decode(&global.get(MARKER).await?), Some(unfinished));
	assert_eq!(calls.get(), 4);
	global.remove(MARKER);

	assert!(
		run(db, || ready(Err!("candidate decode failure")))
			.await
			.is_none()
	);

	assert_absent(db, "global", MARKER, "interrupted run remains retryable").await;
	assert!(
		run(db, async || {
			global.get("unavailable_candidate").await?;
			Ok(Outcome::clean())
		})
		.await
		.is_none()
	);

	assert_eq!(stamp(db), Some(Outcome::clean()));
	assert_eq!(Outcome::decode(&global.get(MARKER).await?), Some(Outcome::clean()));
	global.remove(MARKER);

	global.insert(MARKER, Outcome::clean().encode()?);
	let readonly_server = readonly_server(server)?;
	let readonly = Database::open(&readonly_server).await?;

	assert!(Boundary::writable(&readonly).is_none());
	assert_eq!(calls.get(), 4);
	let rejected =
		Txn::insert(&readonly["global"], [(b"rejected_readonly", b"value")]).try_execute();

	assert!(matches!(rejected, Err(TxnError::Write(_))));
	assert_absent(&readonly, "global", b"rejected_readonly", "read-only write was rejected")
		.await;

	Ok(())
}

#[test]
fn marker_decode_contract() -> Result {
	let clean = Outcome::clean();
	let bytes = clean.encode()?;

	assert_eq!(Outcome::decode(&bytes), Some(clean));
	// A legacy decline record must never read as this repair's completion.
	assert_ne!(MARKER, "fix_short_injectivity");
	assert!(Outcome::decode(b"").is_none());
	assert!(Outcome::decode(b"declined").is_none());
	assert!(Outcome::decode(br#"{"version":2,"status":"clean","counts":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"samples":[],"truncated":false}"#).is_none());
	assert!(Outcome::decode(br#"{"version":1,"status":"clean"}"#).is_none());

	let sample = Sample {
		shape: Shape::DiffCollision,
		identity: b"state".as_slice().into(),
		reason: Reason::HistoricalState,
	};

	let unfinished = Outcome {
		status: Status::Unfinished,
		counts: [1; 15],
		samples: [sample.clone()].into(),
		truncated: true,
		..Outcome::clean()
	};

	let bytes = unfinished.encode()?;

	assert_eq!(Outcome::decode(&bytes), Some(unfinished.clone()));
	let samples = repeat_with(|| sample.clone())
		.take(SAMPLE_LIMIT + 1)
		.collect();

	let oversampled = Outcome { samples, ..unfinished };

	oversampled
		.encode()
		.expect_err("diagnostic sample cap");

	let samples = [Sample {
		identity: [0; IDENTITY_LIMIT + 1].as_slice().into(),
		..sample
	}]
	.into();

	let oversized = Outcome { samples, ..unfinished };

	oversized
		.encode()
		.expect_err("diagnostic identity byte cap");

	Ok(())
}

async fn assert_absent<K>(db: &Database, map: &str, key: &K, message: &str)
where
	K: AsRef<[u8]> + Debug + Sync + ?Sized,
{
	assert!(db[map].get(key).await.is_missing(), "{message}");
}

fn readonly_server(server: &Server) -> Result<Arc<Server>> {
	open_server(server, "rocksdb_read_only")
}

fn open_server(server: &Server, mode: &str) -> Result<Arc<Server>> {
	let raw = Figment::new()
		.merge(("server_name", server.config.server_name.as_str()))
		.merge(("database_path", &server.config.database_path))
		.merge((mode, true));

	let config = Config::new(&raw)?;
	let logging = Logging {
		subscriber: server.log.subscriber.clone(),
		reload: LogLevelReloadHandles::default(),
		capture: server.log.capture.clone(),
	};

	let server = Server::new(
		config,
		Sources::default(),
		Some(server.runtime()),
		logging,
		server.metrics.clone(),
	);

	Ok(Arc::new(server))
}
