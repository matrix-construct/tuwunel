mod extensions;
mod filter;
mod range;
mod rooms;
mod selector;

#[cfg(test)]
mod tests;

use std::{
	collections::{BTreeMap, BTreeSet},
	fmt::Debug,
	iter::{once, repeat},
	sync::Arc,
};

use axum::extract::{Extension, State};
use futures::{FutureExt, TryFutureExt, future::join};
use ruma::{
	DeviceId, OwnedRoomId, RoomId, UserId,
	api::client::sync::sync_events::v5::{ListId, Request, Response, response},
	events::{AnyStrippedStateEvent, StateEventType, room::member::MembershipState},
	serde::Raw,
};
use serde::Deserialize;
use tokio::{sync::Notify, time::timeout_at};
use tuwunel_core::{
	Err, Result, debug,
	debug::INFO_SPAN_LEVEL,
	debug_warn,
	error::inspect_log,
	smallvec::SmallVec,
	trace,
	utils::{
		TryFutureExtExt,
		result::{FlatOk, NotFound},
		time::millis,
	},
	warn,
};
use tuwunel_service::{
	Services,
	presence::Ping,
	rooms::state_cache::StrippedRoomState,
	sync::{Connection, ConnectionKey, ConnectionVal, into_connection_key},
};

use self::{
	extensions::{apply_profiles, apply_ranges, handle as handle_extensions},
	range::collect as collect_ranges,
};
use super::share_encrypted_room;
use crate::{ClientIp, Ruma, utils::poll_deadline};

#[derive(Copy, Clone)]
struct SyncInfo<'a> {
	services: &'a Services,
	sender_user: &'a UserId,
	sender_device: Option<&'a DeviceId>,
	previous_connection_pos: Option<u64>,

	/// The rooms the user calls direct, read once for the whole pass.
	///
	/// Both the list filters and every room payload answer `is_dm` from this.
	direct_rooms: &'a BTreeSet<OwnedRoomId>,
}

#[derive(Clone, Debug)]
struct WindowRoom {
	room_id: OwnedRoomId,
	membership: Option<MembershipState>,
	lists: ListIds,
	event_count: u64,
	payload_count: u64,
}

impl WindowRoom {
	#[inline]
	fn payload_is_fresh(&self, roomsince: u64) -> bool {
		roomsince == 0 || self.payload_count > roomsince
	}
}

type Window = BTreeMap<OwnedRoomId, WindowRoom>;
type ResponseLists = BTreeMap<ListId, response::List>;
type ListIds = SmallVec<[ListId; 1]>;

/// `POST /_matrix/client/unstable/org.matrix.simplified_msc3575/sync`
/// ([MSC4186])
///
/// A simplified version of sliding sync ([MSC3575]).
///
/// Get all new events in a sliding window of rooms since the last sync or a
/// given point in time.
///
/// [MSC3575]: https://github.com/matrix-org/matrix-spec-proposals/pull/3575
/// [MSC4186]: https://github.com/matrix-org/matrix-spec-proposals/pull/4186
#[tracing::instrument(
	name = "sync",
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(
		user_id = %body.sender_user().localpart(),
		device_id = %body.sender_device.as_deref().map_or("<no device>", |x| x.as_str()),
		conn_id = ?body.body.conn_id.clone().unwrap_or_default(),
		since = ?body.body.pos.clone().unwrap_or_default(),
	)
)]
pub(crate) async fn sync_events_v5_route(
	Extension(interrupted): Extension<Arc<Notify>>,
	ClientIp(client): ClientIp,
	State(ref services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let sender_user = body.sender_user();
	let sender_device = body.sender_device.as_deref();
	let request = &body.body;

	if let Some(fields) = request.extensions.profiles.fields.as_deref() {
		services.profile.check_requested_fields(fields)?;
	}

	let since = request
		.pos
		.as_ref()
		.and_then(|string| string.parse().ok())
		.unwrap_or(0);

	let timeout = poll_timeout(request, services.config.client.sync.timeout_max);

	let conn_key = into_connection_key(sender_user, sender_device, request.conn_id.as_deref());
	let conn_val = services
		.sync
		.load_or_init_connection(&conn_key)
		.await?;

	let conn = conn_val.lock();
	let ping = Ping {
		device_id: sender_device,
		client_ip: Some(client),
		new_state: Some(&request.set_presence),
		appservice: body.appservice_info.as_ref(),
	};

	let ping_presence = services
		.presence
		.maybe_ping_presence(sender_user, ping)
		.inspect_err(inspect_log)
		.ok();

	let (mut conn, _) = join(conn, ping_presence).await;

	if since != 0 {
		check_profile_selection(services, request, &conn)?;
	}

	if since != 0 && conn.next_batch == 0 {
		return Err!(Request(UnknownPos(warn!("Connection lost; restarting sync stream."))));
	}

	if since == 0 {
		*conn = Connection::default();
		services
			.sync
			.store_connection(&conn_key, &conn_val, &conn)
			.await?;

		debug_warn!(?conn_key, "Client cleared cache and reloaded.");
	}

	let advancing = since == conn.next_batch;
	let retarding = since != 0 && since <= conn.globalsince;
	if !advancing && !retarding {
		return Err!(Request(UnknownPos(warn!(
			"Requesting unknown or invalid stream position."
		))));
	}

	debug_assert!(
		advancing || retarding,
		"Request should either be advancing or replaying the since token."
	);

	// Update parameters regardless of replay or advance
	conn.next_batch = services.globals.wait_pending().await?;
	conn.globalsince = since.min(conn.next_batch);
	let config_changed = conn.update_cache(request);
	let caught_up = conn.globalsince == conn.next_batch;

	// A whole profile owed to a caught-up connection needs a pass like a new list.
	let needs_pass = config_changed || conn.own_profile_owed();

	if config_change_needs_position(needs_pass, advancing, caught_up, since) {
		// The permit publishes the reserved position when it retires on drop.
		drop(services.globals.next_count());
	}

	conn.update_rooms_prologue(retarding.then_some(since));

	let mut response = Response {
		txn_id: request.txn_id.clone(),
		lists: Default::default(),
		pos: Default::default(),
		rooms: Default::default(),
		extensions: Default::default(),
	};

	let stop_at = poll_deadline(timeout);

	for woken in once(false).chain(repeat(true)) {
		debug_assert!(
			conn.globalsince <= conn.next_batch,
			"since should not be greater than next_batch."
		);

		let window;
		let watchers = services
			.sync
			.watch(sender_user, sender_device, services.state_cache.rooms_joined(sender_user))
			.await;

		conn.next_batch = services.globals.wait_pending().await?;
		let direct_rooms = services
			.account_data
			.direct_rooms(sender_user)
			.await;

		let sync_info = SyncInfo {
			services,
			sender_user,
			sender_device,
			previous_connection_pos: since.ne(&0).then_some(since),
			direct_rooms: &direct_rooms,
		};

		(window, response.lists) = selector::selector(&mut conn, sync_info)
			.boxed()
			.await;

		if conn.globalsince < conn.next_batch {
			let ranges = collect_ranges(sync_info, &conn, &window);
			let extensions = handle_extensions(sync_info, &conn, &window);
			let (mut ranges, extensions) = join(ranges, extensions).boxed().await;

			let mut extensions = extensions?;

			apply_profiles(sync_info, &conn, &window, &ranges, &mut extensions).await?;
			apply_ranges(&conn, &window, &mut ranges, &mut extensions);
			conn.update_rooms_epilogue(since, ranges.room_updates());
			conn.update_profiles_epilogue();
			response.rooms = ranges.into_payloads();
			response.extensions = extensions.into_response(&response.rooms);

			if !is_empty_response(&mut response, woken) {
				return respond(services, (&conn_key, &conn_val), &conn, response).await;
			}
		}

		let waiter = async || {
			tokio::select! {
				() = interrupted.notified() => true,
				watch = timeout_at(stop_at, watchers) => watch.is_err(),
			}
		};

		if timeout == 0 || services.server.is_stopping() || waiter().boxed().await {
			return respond(services, (&conn_key, &conn_val), &conn, response).await;
		}

		debug!(
			?timeout,
			last_since = conn.globalsince,
			last_batch = conn.next_batch,
			pend_count = ?services.globals.pending_count(),
			"notified by watcher"
		);

		conn.globalsince = conn.next_batch;
	}

	unreachable!("a long poll has no last pass")
}

/// How long a request may long-poll, in milliseconds.
///
/// Per MSC4186 the timeout of a request without `pos` is ignored, so opening
/// a connection answers after its first pass.
fn poll_timeout(request: &Request, max: u64) -> u64 {
	request
		.timeout
		.filter(|_| request.pos.is_some())
		.and_then(millis)
		.map(|timeout| timeout.min(max))
		.unwrap_or(0)
}

fn check_profile_selection(services: &Services, request: &Request, conn: &Connection) -> Result {
	let fields = request
		.extensions
		.profiles
		.fields
		.as_deref()
		.or(conn.extensions.profiles.fields.as_deref());

	if let Some(fields) = fields {
		services.profile.check_requested_fields(fields)?;
	}

	Ok(())
}

fn config_change_needs_position(
	config_changed: bool,
	advancing: bool,
	caught_up: bool,
	since: u64,
) -> bool {
	config_changed && advancing && caught_up && since != 0
}

/// Whether a pass has nothing to answer the client with.
///
/// The e2ee extension reports the device's unused fallback key types on every
/// pass. They are current state, and any activity on the server moves the
/// global count that admits a first pass, so they never end one. A pass the
/// user's watchers woke, on activity in their rooms or on their account, does
/// answer with them, since matrix-rust-sdk backs up the room keys it has just
/// made on that answer. The response is left as it was, so the types are still
/// delivered.
fn is_empty_response(response: &mut Response, woken: bool) -> bool {
	let fallback_key_types = response
		.extensions
		.e2ee
		.device_unused_fallback_key_types
		.take(); // Extensions::is_empty cannot skip one field

	let empty = response.extensions.is_empty()
		&& response.rooms.is_empty()
		&& (!woken || fallback_key_types.is_none());

	response
		.extensions
		.e2ee
		.device_unused_fallback_key_types = fallback_key_types;

	empty
}

/// Positions a response at the connection's next batch and stores the
/// connection.
///
/// The connection is stored as this response leaves it, so the client's next
/// request resumes from the position the response carries.
async fn respond(
	services: &Services,
	(key, loaded): (&ConnectionKey, &ConnectionVal),
	conn: &Connection,
	response: Response,
) -> Result<Response> {
	let response = Response {
		pos: conn.next_batch.to_string().into(),
		..response
	};

	trace!(conn.globalsince, conn.next_batch, ?response, "Responding");

	services
		.sync
		.store_connection(key, loaded, conn)
		.await?;

	Ok(response)
}

/// The stripped state MSC4186 sends for an invited or knocked room.
///
/// For a knocked room this is the summary stored with the knock, ending with
/// the user's own knock event, so a client can present a room this server is
/// not in. An empty knock summary is omitted, since a client reading the
/// user's membership from it would take the room for an invite.
#[tracing::instrument(level = "trace", skip_all)]
async fn stripped_state(
	services: &Services,
	sender_user: &UserId,
	room_id: &RoomId,
	membership: Option<&MembershipState>,
) -> StrippedRoomState {
	let membership = membership?;
	let state_cache = &services.state_cache;
	let state = match membership {
		| MembershipState::Invite =>
			state_cache
				.invite_state(sender_user, room_id)
				.await,
		| MembershipState::Knock =>
			state_cache
				.knock_state(sender_user, room_id)
				.await,
		| _ => return None,
	};

	state
		.present()
		.inspect_err(|error| warn!(%room_id, %error, "Unreadable stripped room state"))
		.flat_ok()
		.filter(|state| !state.is_empty() || *membership == MembershipState::Invite)
}

/// The content of the stripped state event with this type and an empty state
/// key.
///
/// The first match wins; an event whose type or state key does not parse is
/// skipped.
fn stripped_content<'a, T>(
	state: &'a [Raw<AnyStrippedStateEvent>],
	kind: &StateEventType,
) -> Option<T>
where
	T: Deserialize<'a>,
{
	state
		.iter()
		.filter(|event| {
			event
				.get_field("type")
				.flat_ok()
				.is_some_and(|event_type: StateEventType| event_type == *kind)
		})
		.find(|event| {
			event
				.get_field("state_key")
				.flat_ok()
				.is_some_and(str::is_empty)
		})?
		.get_field("content")
		.flat_ok()
}
