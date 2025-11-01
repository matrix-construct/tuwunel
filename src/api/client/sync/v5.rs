mod extensions;
mod filter;
mod rooms;
mod selector;

use std::{collections::BTreeMap, fmt::Debug, time::Duration};

use axum::extract::State;
use futures::{
	FutureExt, TryFutureExt,
	future::{join, try_join},
};
use ruma::{
	DeviceId, OwnedRoomId, UserId,
	api::client::sync::sync_events::v5::{ListId, Request, Response, response},
	events::room::member::MembershipState,
};
use tokio::time::{Instant, timeout_at};
use tuwunel_core::{
	Err, Result, debug,
	debug::INFO_SPAN_LEVEL,
	debug_warn,
	error::inspect_log,
	smallvec::SmallVec,
	trace,
	utils::{TryFutureExtExt, result::FlatOk},
};
use tuwunel_service::{
	Services,
	sync::{Connection, into_connection_key},
};

use super::share_encrypted_room;
use crate::Ruma;

#[derive(Copy, Clone)]
struct SyncInfo<'a> {
	services: &'a Services,
	sender_user: &'a UserId,
	sender_device: &'a DeviceId,
}

#[derive(Clone, Debug)]
struct WindowRoom {
	room_id: OwnedRoomId,
	membership: Option<MembershipState>,
	lists: ListIds,
	ranked: usize,
	last_count: u64,
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
		device_id = %body.sender_device(),
		conn_id = ?body.body.conn_id.clone().unwrap_or_default(),
		since = ?body.body.pos.clone().unwrap_or_default(),
	)
)]
pub(crate) async fn sync_events_v5_route(
	State(ref services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let (sender_user, sender_device) = body.sender();
	let request = &body.body;
	let since = request
		.pos
		.as_ref()
		.and_then(|string| string.parse().ok())
		.unwrap_or(0);

	let timeout = request
		.timeout
		.as_ref()
		.map(Duration::as_millis)
		.map(TryInto::try_into)
		.flat_ok()
		.map(|timeout: u64| {
			timeout
				.max(services.config.client_sync_timeout_min)
				.min(services.config.client_sync_timeout_max)
		})
		.unwrap_or(0);

	let conn_key = into_connection_key(sender_user, sender_device, request.conn_id.as_deref());
	let conn_val = services
		.sync
		.load_or_init_connection(&conn_key)
		.await;

	let conn = conn_val.lock();
	let ping_presence = services
		.presence
		.maybe_ping_presence(sender_user, Some(sender_device), &request.set_presence)
		.inspect_err(inspect_log)
		.ok();

	let (mut conn, _) = join(conn, ping_presence).await;

	if since != 0 && conn.next_batch == 0 {
		return Err!(Request(UnknownPos(warn!("Connection lost; restarting sync stream."))));
	}

	if since == 0 {
		*conn = Connection::default();
		conn.store(&services.sync, &conn_key);
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
	conn.update_cache(request);
	conn.update_rooms_prologue(retarding.then_some(since));

	let mut response = Response {
		txn_id: request.txn_id.clone(),
		lists: Default::default(),
		pos: Default::default(),
		rooms: Default::default(),
		extensions: Default::default(),
	};

	let stop_at = Instant::now()
		.checked_add(Duration::from_millis(timeout))
		.expect("configuration must limit maximum timeout");

	let sync_info = SyncInfo { services, sender_user, sender_device };
	loop {
		debug_assert!(
			conn.globalsince <= conn.next_batch,
			"since should not be greater than next_batch."
		);

		let window;
		let watchers = services.sync.watch(
			sender_user,
			sender_device,
			services.state_cache.rooms_joined(sender_user),
		);

		conn.next_batch = services.globals.wait_pending().await?;
		(window, response.lists) = selector::selector(&mut conn, sync_info)
			.boxed()
			.await;

		if conn.globalsince < conn.next_batch {
			let rooms = rooms::handle(sync_info, &conn, &window)
				.map_ok(|response_rooms| response.rooms = response_rooms);

			let extensions = extensions::handle(sync_info, &conn, &window)
				.map_ok(|response_extensions| response.extensions = response_extensions);

			try_join(rooms, extensions).boxed().await?;

			conn.update_rooms_epilogue(window.keys().map(AsRef::as_ref));

			if !is_empty_response(&response) {
				response.pos = conn.next_batch.to_string().into();
				trace!(conn.globalsince, conn.next_batch, "response {response:?}");
				conn.store(&services.sync, &conn_key);
				return Ok(response);
			}
		}

		if timeout == 0
			|| services.server.is_stopping()
			|| timeout_at(stop_at, watchers)
				.boxed()
				.await
				.is_err()
		{
			response.pos = conn.next_batch.to_string().into();
			trace!(conn.globalsince, conn.next_batch, "timeout; empty response {response:?}");
			conn.store(&services.sync, &conn_key);
			return Ok(response);
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
}

fn is_empty_response(response: &Response) -> bool {
	response.extensions.is_empty() && response.rooms.is_empty()
}
