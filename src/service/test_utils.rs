use std::{
	env::{current_exe, temp_dir, var_os},
	fs::remove_dir_all,
	path::{Path, PathBuf},
	pin::{Pin, pin},
	sync::{Arc, Mutex},
	task::{Context, Poll, Waker},
	thread::current as current_thread,
};

use futures::future::ready;
use ruma::{CanonicalJsonObject, EventId, RoomId, UserId, events::StateEventType};
use serde::Serialize;
use serde_json::{Value as JsonValue, from_value, json};
use tokio::{process::Command, runtime::Handle};
use tracing::{
	Level,
	dispatcher::DefaultGuard,
	subscriber::{NoSubscriber, set_default},
};
use tracing_subscriber::{layer::SubscriberExt, registry};
use tuwunel_core::{
	Err, Result, Server,
	config::{Config, Figment, Sources},
	err, implement,
	log::{
		LogLevelReloadHandles, Logging,
		capture::{Capture, Data, Guard, Layer, State, Value},
	},
	metrics::Metrics,
	utils::random_string,
};
use tuwunel_matrix::{PduCount, PduId, RawPduId};

use crate::{Services, rooms::state_compressor::CompressedStateEvent};

pub(crate) struct Fixture {
	pub services: Arc<Services>,
}

struct DatabasePath(PathBuf);

type Fields = Vec<Value>;

type Recorded = Arc<Mutex<Vec<Fields>>>;

/// Records the fields of every warning the calling thread emits.
///
/// Recording lasts while the value lives; events at other levels, ERROR
/// included, are not recorded.
pub(crate) struct Warnings {
	recorded: Recorded,
	_capture: Guard,
	_subscriber: DefaultGuard,
}

const CHILD_TEST_ENV: &str = "SERVICE_TEST_CHILD";
const CHILD_DATABASE_ENV: &str = "SERVICE_TEST_DATABASE";

/// Runs the calling test in a child process and builds its services there.
///
/// The parent returns `None` after checking the child's result. Service references
/// form cycles, so only process exit releases their database handles.
pub(crate) async fn fixture(config: Figment) -> Result<Option<Fixture>> {
	let thread = current_thread();
	let name = thread
		.name()
		.ok_or_else(|| err!("service fixture requires a named test thread"))?;

	match (var_os(CHILD_TEST_ENV), var_os(CHILD_DATABASE_ENV)) {
		| (None, None) => {
			run_test(name).await?;
			Ok(None)
		},
		| (Some(child), Some(path)) if child == name => {
			let fixture = build(config, Path::new(&path)).await?;

			Ok(Some(fixture))
		},
		| _ => Err!("service fixture child markers do not match {name}"),
	}
}

async fn run_test(name: &str) -> Result {
	let path = temp_dir().join("tuwunel").join(random_string(32));
	let status = Command::new(current_exe()?)
		.arg(concat!("-", "-exact"))
		.arg(name)
		.env(CHILD_TEST_ENV, name)
		.env(CHILD_DATABASE_ENV, &path)
		.kill_on_drop(true)
		.status()
		.await?;

	// Arm cleanup only after reaping; cancellation must not remove an open database.
	let path = DatabasePath(path);

	if !status.success() {
		return Err!("isolated service test {name} failed with {status}");
	}

	if !path.0.is_dir() {
		return Err!("isolated service test {name} did not create its fixture");
	}

	Ok(())
}

async fn build(config: Figment, path: &Path) -> Result<Fixture> {
	let raw = Figment::new()
		.merge(("server_name", "localhost"))
		.merge(config)
		.merge(("database_path", path.to_string_lossy().as_ref()));

	let config = Config::new(&raw)?;
	let runtime = Handle::current();
	let logging = Logging {
		subscriber: Arc::new(NoSubscriber::new()),
		reload: LogLevelReloadHandles::default(),
		capture: Arc::new(State::new()),
	};

	let metrics = Metrics::new(Some(&runtime));
	let server =
		Arc::new(Server::new(config, Sources::default(), Some(&runtime), logging, metrics));

	let services = Services::build(server).await?;

	Ok(Fixture { services })
}

pub(crate) fn pdu_id(count: u64) -> RawPduId {
	PduId {
		shortroomid: 1,
		count: PduCount::Normal(count),
	}
	.into()
}

/// Builds a minimal PDU as JSON for seeding storage.
///
/// The event carries a placeholder hash and no signatures or references, so it
/// deserializes as a PDU without passing authorization.
pub(crate) fn pdu_json<Kind>(
	event_id: &EventId,
	room_id: &RoomId,
	sender: &UserId,
	kind: &Kind,
	content: &JsonValue,
) -> JsonValue
where
	Kind: Serialize + ?Sized,
{
	json!({
		"event_id": event_id, "room_id": room_id, "sender": sender, "type": kind,
		"content": content, "origin": "localhost", "origin_server_ts": 1, "depth": 1,
		"prev_events": [], "auth_events": [], "signatures": {},
		"hashes": {"sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"},
	})
}

/// Sets the state key of an event built by [`pdu_json`].
///
/// Panics if the event is neither a JSON object nor null.
pub(crate) fn with_state_key(mut pdu: JsonValue, state_key: &str) -> JsonValue {
	pdu["state_key"] = json!(state_key);
	pdu
}

/// Stores a JSON event as an outlier and compresses its state entry.
///
/// The event need not parse as a PDU, as with the stripped state a knock stores.
pub(crate) async fn store_outlier(
	services: &Services,
	event_type: &StateEventType,
	state_key: &str,
	event_id: &EventId,
	pdu: JsonValue,
) -> Result<CompressedStateEvent> {
	let pdu: CanonicalJsonObject = from_value(pdu)?;

	services.timeline.add_pdu_outlier(event_id, &pdu);

	Ok(compress_state(services, event_type, state_key, event_id).await)
}

/// Compresses one state entry, creating its short IDs when missing.
///
/// The entry pairs the interned state key with the event's short ID.
pub(crate) async fn compress_state(
	services: &Services,
	event_type: &StateEventType,
	state_key: &str,
	event_id: &EventId,
) -> CompressedStateEvent {
	let shortstatekey = services
		.short
		.get_or_create_shortstatekey(event_type, state_key)
		.await;

	services
		.state_compressor
		.compress_state_event(shortstatekey, event_id)
		.await
}

/// Polls a pinned future once with `waker` and returns the result.
///
/// Watch tests poll a subscriber before and after a write to see whether the
/// write woke it.
pub(crate) fn poll_once<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
	future.poll(&mut Context::from_waker(waker))
}

/// Asserts that a future waits while a guard is held, then releases the guard
/// and awaits the future.
///
/// Lock tests start a writer while holding the lock it needs, and expect it to
/// finish once the lock is free.
pub(crate) async fn released_by<Guard, F: Future>(guard: Guard, future: F) -> F::Output {
	released_after(guard, ready(()), future).await
}

/// Asserts that a future waits while a guard is held, runs `meanwhile` as the
/// guard's holder, then releases the guard and awaits the future.
///
/// Lock tests change or check the guarded state in `meanwhile`, so a writer
/// that read or wrote it before taking the lock is caught.
pub(crate) async fn released_after<Guard, M, F>(
	guard: Guard,
	meanwhile: M,
	future: F,
) -> F::Output
where
	M: Future<Output = ()>,
	F: Future,
{
	let future = pin!(future);
	let future = assert_pending(future);

	meanwhile.await;
	drop(guard);
	future.await
}

fn assert_pending<F: Future>(mut future: Pin<&mut F>) -> Pin<&mut F> {
	assert!(
		poll_once(future.as_mut(), Waker::noop()).is_pending(),
		"must wait for the guard"
	);

	future
}

/// Starts recording warnings on the calling thread.
///
/// The capture layer replaces the calling thread's default subscriber until the
/// value is dropped.
#[implement(Warnings)]
pub(crate) fn capture() -> Self {
	let logs = Arc::new(State::new());
	let recorded: Recorded = Arc::default();
	let sink = recorded.clone();
	let is_warning = |data: Data<'_>| data.level() == Level::WARN;
	let capture = Capture::new(&logs, Some(is_warning), move |data: Data<'_>| {
		sink.lock()
			.expect("locked")
			.push(data.values.to_vec());
	});

	Self {
		recorded,
		_capture: capture.start(),
		_subscriber: set_default(registry().with(Layer::new(&logs))),
	}
}

#[implement(Warnings)]
#[inline]
pub(crate) fn count(&self) -> usize { self.recorded.lock().expect("locked").len() }

/// Reports whether any recorded warning carries the named field with the given
/// value.
///
/// The value is compared with the field's formatted text, and an event's
/// message is recorded under the `message` field name.
#[implement(Warnings)]
pub(crate) fn warned(&self, name: &str, value: &str) -> bool {
	self.recorded
		.lock()
		.expect("locked")
		.iter()
		.flatten()
		.any(|(field, text)| *field == name && text == value)
}

impl Drop for DatabasePath {
	fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
}
