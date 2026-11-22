use std::{
	env::{current_exe, temp_dir, var_os},
	fs::remove_dir_all,
	path::{Path, PathBuf},
	pin::{Pin, pin},
	process::Command as StdCommand,
	slice::from_ref,
	sync::{Arc, Mutex},
	task::{Context, Poll, Waker},
	thread::current as current_thread,
};

use futures::future::ready;
use ruma::{
	CanonicalJsonObject, DeviceId, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId,
	RoomVersionId, ServerName, UserId,
	events::{
		AnyStrippedStateEvent, StateEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	serde::{Base64, Raw},
	signatures::{Ed25519KeyPair, hash_and_sign_event},
};
use serde::Serialize;
use serde_json::{Value as JsonValue, from_value, json, to_value, to_vec};
use tokio::{process::Command, runtime::Handle, task::coop::unconstrained};
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
	utils::{random_string, sys::command_without_env_prefixes},
};
use tuwunel_matrix::{
	PduCount, PduId, RawPduId, event::gen_event_id, room_version::rules as room_version_rules,
};

use crate::{
	Services,
	rooms::{
		short::ShortStateHash,
		state_cache::MembershipUpdate,
		state_compressor::{CompressedState, CompressedStateEvent},
	},
	users,
};

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

/// Describes one v11 state event for `state_event` to sign.
///
/// The event is sent by `@bob:localhost` and signed with this server's key.
pub(crate) struct StateEvent<'a> {
	pub(crate) kind: &'a str,
	pub(crate) state_key: &'a str,
	pub(crate) content: &'a JsonValue,
	pub(crate) auth_events: &'a [&'a OwnedEventId],
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
	let command = StdCommand::new(current_exe()?);
	let command = command_without_env_prefixes(command, &["CONDUIT_", "CONDUWUIT_", "TUWUNEL_"]);
	let status = Command::from(command)
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
pub(crate) fn with_state_key(pdu: JsonValue, state_key: &str) -> JsonValue {
	with_field(pdu, "state_key", json!(state_key))
}

/// Sets one top-level field of a JSON event.
///
/// A signed event keeps its hashes and signatures, so it no longer verifies
/// unless redaction strips the field. Panics if the event is neither a JSON
/// object nor null.
pub(crate) fn with_field(mut pdu: JsonValue, key: &str, value: JsonValue) -> JsonValue {
	pdu[key] = value;
	pdu
}

/// Builds a v11 state event signed by this server.
///
/// The event carries a real content hash and signature, so it verifies with
/// this server's active key. It is returned in its wire form with its derived
/// ID.
pub(crate) fn signed_state(
	services: &Services,
	room_id: &RoomId,
	kind: &str,
	state_key: &str,
	content: &JsonValue,
) -> Result<(OwnedEventId, JsonValue)> {
	let version = RoomVersionId::V11;
	let sender = format!("@bob:{}", services.globals.server_name());
	let event = json!({
		"room_id": room_id, "sender": sender, "type": kind, "state_key": state_key,
		"content": content, "origin_server_ts": 1, "depth": 1, "prev_events": [],
		"auth_events": [],
	});

	signed(services, &version, event)
}

/// Signs an event with this server's key and derives its ID, both under the
/// given room version.
///
/// The event comes back in its wire form beside the derived ID.
pub(crate) fn signed(
	services: &Services,
	version: &RoomVersionId,
	event: JsonValue,
) -> Result<(OwnedEventId, JsonValue)> {
	let event = sign_event(services, version, from_value(event)?)?;
	let event_id = gen_event_id(&event, version)?;

	Ok((event_id, to_value(event)?))
}

fn sign_event(
	services: &Services,
	version: &RoomVersionId,
	mut event: CanonicalJsonObject,
) -> Result<CanonicalJsonObject> {
	services
		.server_keys
		.hash_and_sign_event(&mut event, version)?;

	Ok(event)
}

/// Builds the create event of a v11 room and the join of its creator,
/// `@bob:localhost`.
///
/// The create event comes first and the join second, each beside its event ID.
pub(crate) fn room(
	services: &Services,
	room_id: &RoomId,
) -> Result<[(OwnedEventId, JsonValue); 2]> {
	let create = create_event(services, room_id, &json!({ "room_version": "11" }))?;
	let join = state_event(services, room_id, StateEvent {
		kind: "m.room.member",
		state_key: "@bob:localhost",
		content: &json!({ "membership": "join" }),
		auth_events: &[&create.0],
	})?;

	Ok([create, join])
}

/// Builds a second create event for a room built by [`room`].
///
/// It sets `m.federate`, which the first leaves out, so the two differ.
pub(crate) fn other_create(
	services: &Services,
	room_id: &RoomId,
) -> Result<(OwnedEventId, JsonValue)> {
	create_event(services, room_id, &json!({ "room_version": "11", "m.federate": false }))
}

fn create_event(
	services: &Services,
	room_id: &RoomId,
	content: &JsonValue,
) -> Result<(OwnedEventId, JsonValue)> {
	state_event(services, room_id, StateEvent {
		kind: "m.room.create",
		state_key: "",
		content,
		auth_events: &[],
	})
}

/// Builds a join for a room's creator sent by `@mallory:remote.example`,
/// following only the room's create event.
///
/// The room version's rules allow that shape before they check that a join is
/// self-sent. The event is returned unsigned.
pub(crate) fn creator_join(
	room_id: &RoomId,
	creator: &str,
	create_id: &OwnedEventId,
) -> JsonValue {
	json!({
		"type": "m.room.member",
		"state_key": creator,
		"content": { "membership": "join" },
		"room_id": room_id,
		"sender": "@mallory:remote.example",
		"origin_server_ts": 1,
		"depth": 2,
		"prev_events": [create_id],
		"auth_events": [create_id],
	})
}

/// Signs a v11 state event of `@bob:localhost` with this server's key, as
/// another server would relay it.
///
/// Its previous event is the last of its auth events.
pub(crate) fn state_event(
	services: &Services,
	room_id: &RoomId,
	event: StateEvent<'_>,
) -> Result<(OwnedEventId, JsonValue)> {
	let prev_events = event
		.auth_events
		.last()
		.map(from_ref)
		.unwrap_or_default();

	signed_event(services, room_id, event, prev_events)
}

/// Signs a v11 state event of `@bob:localhost` naming the given previous
/// events.
///
/// Its depth is one more than the number of its auth events.
pub(crate) fn signed_event(
	services: &Services,
	room_id: &RoomId,
	StateEvent { kind, state_key, content, auth_events }: StateEvent<'_>,
	prev_events: &[&OwnedEventId],
) -> Result<(OwnedEventId, JsonValue)> {
	let event = json!({
		"type": kind,
		"state_key": state_key,
		"content": content,
		"room_id": room_id,
		"sender": "@bob:localhost",
		"origin_server_ts": 1,
		"depth": auth_events.len().saturating_add(1),
		"prev_events": prev_events,
		"auth_events": auth_events,
	});

	signed(services, &RoomVersionId::V11, event)
}

/// Generates a signing key for another server and stores its verify key, as
/// fetching that server's keys would.
///
/// Events the server signs with the key then verify here without a fetch.
pub(crate) fn remote_key(services: &Services, origin: &ServerName) -> Result<Ed25519KeyPair> {
	let key = Ed25519KeyPair::from_der(&Ed25519KeyPair::generate(), "remote".into())
		.map_err(|e| err!("{origin} key: {e}"))?;

	let key_id = format!("ed25519:{}", key.version());
	let public_key: Base64 = Base64::new(key.public_key().to_vec());
	let keys = json!({
		"server_name": origin,
		"valid_until_ts": MilliSecondsSinceUnixEpoch::now(),
		"verify_keys": { key_id: { "key": public_key } },
		"old_verify_keys": {},
		"signatures": {},
	});

	services.db["server_signingkeys"].insert(origin.as_bytes(), to_vec(&keys)?);

	Ok(key)
}

/// Signs an event as another server and derives its ID, both under the given
/// room version.
///
/// The event comes back in its wire form beside the derived ID.
pub(crate) fn remote_signed(
	origin: &ServerName,
	key: &Ed25519KeyPair,
	version: &RoomVersionId,
	event: JsonValue,
) -> Result<(OwnedEventId, JsonValue)> {
	let event = remote_sign_event(origin, key, version, from_value(event)?)?;
	let event_id = gen_event_id(&event, version)?;

	Ok((event_id, to_value(event)?))
}

fn remote_sign_event(
	origin: &ServerName,
	key: &Ed25519KeyPair,
	version: &RoomVersionId,
	mut event: CanonicalJsonObject,
) -> Result<CanonicalJsonObject> {
	let rules = room_version_rules(version)?;

	hash_and_sign_event(origin.as_str(), key, &mut event, &rules.redaction)
		.map_err(|e| err!("sign event as {origin}: {e}"))?;

	Ok(event)
}

/// Decodes stripped state events as JSON values, for comparing what a client is
/// shown.
///
/// Fails on the first event that is not valid JSON.
pub(crate) fn json_events(state: &[Raw<AnyStrippedStateEvent>]) -> Result<Vec<JsonValue>> {
	state
		.iter()
		.map(|event| {
			event
				.deserialize_as_unchecked()
				.map_err(Into::into)
		})
		.collect()
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

/// Creates a user with one device for each ID given.
///
/// Key and connection writes are skipped for a device that does not exist, so
/// tests of them start from devices created here. Tests pass a few IDs, so the
/// devices are created in turn.
pub(crate) async fn create_devices(
	users: &users::Service,
	user_id: &UserId,
	device_ids: &[&DeviceId],
) -> Result {
	users.create(user_id, None, None).await?;

	for &device_id in device_ids {
		users
			.create_device(user_id, Some(device_id), (None, None), None, None, None)
			.await?;
	}

	Ok(())
}

/// Saves a full state as a snapshot and makes it the room's current state.
///
/// The snapshot is stored like any other before the room's pointer moves to it,
/// and its short state hash comes back.
pub(crate) async fn save_room_state(
	services: &Services,
	room_id: &RoomId,
	state: Arc<CompressedState>,
) -> Result<ShortStateHash> {
	let shortstatehash = services
		.state_compressor
		.save_state(room_id, state)
		.await?
		.shortstatehash;

	set_room_state(services, room_id, shortstatehash).await;

	Ok(shortstatehash)
}

/// Points the room's current state at a snapshot, under the room's state lock.
///
/// The guard drops once the pointer is written, so the caller holds no lock
/// afterwards.
pub(crate) async fn set_room_state(
	services: &Services,
	room_id: &RoomId,
	shortstatehash: ShortStateHash,
) {
	let lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_room_state(room_id, shortstatehash, &lock);
}

/// Builds a state-cache membership update with no event behind it.
///
/// It is the user's own transition to a bare membership, with the defaults of
/// `MembershipUpdate::self_sent`. A caller taking the count from `next_count`
/// must hold its permit until the update is awaited, so the row lands before
/// the count retires.
pub(crate) fn membership_update<'a>(
	room_id: &'a RoomId,
	user_id: &'a UserId,
	membership: MembershipState,
	count: u64,
) -> MembershipUpdate<'a> {
	let content = RoomMemberEventContent::new(membership);

	MembershipUpdate::self_sent(room_id, user_id, content, PduCount::Normal(count))
}

/// Polls a pinned future once with `waker`, outside tokio's cooperative budget.
///
/// Watch tests poll a subscriber before and after a write to see whether the
/// write woke it. With the budget spent, a tokio resource would return
/// `Pending` without registering and have the runtime wake `waker` at its next
/// yield, which a probe would take for a notification. Wakes from any other
/// source, the future itself included, still reach `waker`.
pub(crate) fn poll_once<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
	pin!(unconstrained(future)).poll(&mut Context::from_waker(waker))
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

mod tests {
	use std::{
		pin::pin,
		sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		},
		task::{Wake, Waker},
	};

	use tokio::{
		sync::watch::channel,
		task::{consume_budget, coop::has_budget_remaining},
	};

	use super::poll_once;

	#[derive(Default)]
	struct Wakes(AtomicUsize);

	#[tokio::test]
	async fn poll_once_registers_a_watch_with_the_budget_spent() {
		let (sender, mut receiver) = channel(0);
		let wakes = Arc::new(Wakes::default());
		let waker = Waker::from(wakes.clone());
		let mut changed = pin!(receiver.changed()); // first poll reborrows through Pin::as_mut

		while has_budget_remaining() {
			consume_budget().await;
		}

		assert!(poll_once(changed.as_mut(), &waker).is_pending());
		assert!(!has_budget_remaining(), "the probe must leave the spent budget spent");

		sender.send_replace(1);
		assert_eq!(wakes.0.load(Ordering::SeqCst), 1, "the send must wake the probe");
		assert!(poll_once(changed, &waker).is_ready());
	}

	impl Wake for Wakes {
		fn wake(self: Arc<Self>) { self.wake_by_ref(); }

		fn wake_by_ref(self: &Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
	}
}
