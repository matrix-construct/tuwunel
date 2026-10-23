mod backfill;
mod event;
mod event_auth;
mod missing_events;
mod state_ids;

use std::num::NonZeroUsize;

use clap::Subcommand;
use ruma::{OwnedEventId, OwnedRoomId, OwnedServerName};
use tokio::time::Instant;
use tuwunel_core::Result;
use tuwunel_service::fetcher::{Op, Opts};

use crate::{Context, admin_command_dispatch};

/// Drive the federation event-fetcher service directly, one subcommand per
/// federation op. Diagnostic only: validation toggles default off so a
/// malformed or unauthenticatable response is still shown; pass `verify` to
/// turn the full poison-detection path on.
#[admin_command_dispatch(handler_prefix = "event_fetcher")]
#[derive(Debug, Subcommand)]
pub(crate) enum EventFetcherCommand {
	/// `GET /_matrix/federation/v1/event/{eventId}`
	Event {
		room_id: OwnedRoomId,
		event_id: OwnedEventId,

		/// Try this server first.
		#[arg(short, long)]
		server: Option<OwnedServerName>,

		/// Cap the number of servers tried; 0 or unset means unbounded.
		#[arg(long)]
		attempt_limit: Option<usize>,

		/// Verify signature, content hash, event id, and conformance.
		#[arg(long)]
		verify: bool,
	},

	/// `GET /_matrix/federation/v1/event_auth/{roomId}/{eventId}`
	EventAuth {
		room_id: OwnedRoomId,
		event_id: OwnedEventId,

		/// Try this server first.
		#[arg(short, long)]
		server: Option<OwnedServerName>,

		/// Cap the number of servers tried; 0 or unset means unbounded.
		#[arg(long)]
		attempt_limit: Option<usize>,

		/// Verify signature, content hash, event id, and conformance.
		#[arg(long)]
		verify: bool,
	},

	/// `GET /_matrix/federation/v1/backfill/{roomId}` from `event_id`.
	Backfill {
		room_id: OwnedRoomId,
		event_id: OwnedEventId,

		/// Try this server first.
		#[arg(short, long)]
		server: Option<OwnedServerName>,

		/// Cap the number of servers tried; 0 or unset means unbounded.
		#[arg(long)]
		attempt_limit: Option<usize>,

		/// Maximum number of PDUs to request (defaults to 10).
		#[arg(long)]
		limit: Option<usize>,

		/// Verify signature, content hash, event id, and conformance.
		#[arg(long)]
		verify: bool,
	},

	/// `GET /_matrix/federation/v1/state_ids/{roomId}?event_id=`
	StateIds {
		room_id: OwnedRoomId,
		event_id: OwnedEventId,

		/// Try this server first.
		#[arg(short, long)]
		server: Option<OwnedServerName>,

		/// Cap the number of servers tried; 0 or unset means unbounded.
		#[arg(long)]
		attempt_limit: Option<usize>,

		/// Verify signature, content hash, event id, and conformance.
		#[arg(long)]
		verify: bool,
	},

	/// `POST /_matrix/federation/v1/get_missing_events/{roomId}` with
	/// `event_id` as the sole latest event.
	MissingEvents {
		room_id: OwnedRoomId,
		event_id: OwnedEventId,

		/// Try this server first.
		#[arg(short, long)]
		server: Option<OwnedServerName>,

		/// Cap the number of servers tried; 0 or unset means unbounded.
		#[arg(long)]
		attempt_limit: Option<usize>,

		/// Verify signature, content hash, event id, and conformance.
		#[arg(long)]
		verify: bool,
	},
}

/// Validation toggles default off here, inverting `Opts::new`; only `verify`
/// re-enables them.
pub(super) fn base_opts(
	op: Op,
	room_id: OwnedRoomId,
	event_id: OwnedEventId,
	server: Option<OwnedServerName>,
	attempt_limit: Option<usize>,
	verify: bool,
) -> Opts {
	Opts {
		hint: server,
		attempt_limit: attempt_limit.and_then(NonZeroUsize::new),
		check_event_id: verify,
		check_conforms: verify,
		check_hashes: verify,
		check_signature: verify,
		..Opts::new(op, room_id).event_id(event_id)
	}
}

/// Prints the response as pretty JSON, falling back to lossy UTF-8 on parse
/// failure.
pub(super) async fn run(context: &Context<'_>, opts: Opts) -> Result {
	context
		.services
		.server
		.config
		.require_federation()?;

	let timer = Instant::now();
	let outcome = context.services.fetcher.fetch(opts).await?;
	let elapsed = timer.elapsed();
	let len = outcome.bytes.len();
	let origin = &outcome.origin;

	let body = match serde_json::from_slice::<serde_json::Value>(&outcome.bytes) {
		| Ok(value) => serde_json::to_string_pretty(&value)?,
		| Err(_) => String::from_utf8_lossy(&outcome.bytes).into_owned(),
	};

	write!(
		context,
		"Fetched {len} bytes from {origin} in {elapsed:?}.\n\n```json\n{body}\n```"
	)
	.await
}
