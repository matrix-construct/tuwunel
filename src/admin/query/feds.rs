mod head;
mod state;
mod version;

use std::{borrow::Cow, cmp::Ordering, num::NonZeroUsize, time::Duration};

use clap::{Args, Subcommand};
use futures::StreamExt;
use ruma::{OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedUserId};
use tuwunel_core::{Err, Result, utils::stream::ReadyExt};
use tuwunel_service::federation::feds::{Fault, Opts, Record};

use crate::{Context, admin_command_dispatch};

// Feds queries share one serial worker, so larger sweeps require confirmation.
const DESTINATION_LIMIT: usize = 512;

/// Run feds diagnostics against every participating server in a room.
///
/// Each command bounds concurrency and time while reporting every successful
/// response, transport failure, timeout, and undispatched destination.
#[admin_command_dispatch(handler_prefix = "feds")]
#[derive(Debug, Subcommand)]
pub(crate) enum FedsCommand {
	/// Compare implementation versions reported by participating servers.
	Version {
		room: OwnedRoomOrAliasId,

		#[command(flatten)]
		sweep: SweepArgs,
	},

	/// Compare state event identifiers at one room event.
	State {
		room: OwnedRoomOrAliasId,

		/// Anchor the request at this event; the local latest PDU is the
		/// default.
		#[arg(long)]
		at: Option<OwnedEventId>,

		/// Compare the auth-chain identifiers returned with the state response.
		#[arg(long)]
		auth_chain: bool,

		/// Print every identifier in each equivalence class.
		#[arg(long)]
		full: bool,

		#[command(flatten)]
		sweep: SweepArgs,
	},

	/// Probe each server's room head with a make-join request.
	///
	/// A remote may lock room state, sign a template, and persist an otherwise
	/// unused short event identifier while answering this diagnostic request.
	Head {
		room: OwnedRoomOrAliasId,

		/// Use this local user for the probe instead of the diagnostic user.
		#[arg(long)]
		probe_user: Option<OwnedUserId>,

		#[command(flatten)]
		sweep: SweepArgs,
	},
}

/// Resource governors shared by feds queries.
///
/// The defaults are intentionally narrower than the service defaults because
/// a feds query blocks the serial command worker until it settles.
#[derive(Clone, Copy, Debug, Args)]
pub(crate) struct SweepArgs {
	/// Maximum requests in flight.
	#[arg(long, default_value = "16")]
	width: NonZeroUsize,

	/// Per-destination deadline in seconds.
	#[arg(long, default_value_t = 10)]
	timeout: u64,

	/// Whole-sweep budget in seconds.
	#[arg(long, default_value_t = 120)]
	budget: u64,

	/// Remove this homeserver from the destination set.
	#[arg(long)]
	no_loopback: bool,

	/// Confirm a feds query whose destination count exceeds the safety cap.
	#[arg(long)]
	yes_i_want_to_do_this: bool,
}

pub(super) struct Prepared {
	pub(super) room_id: OwnedRoomId,
	pub(super) opts: Opts,
}

pub(super) async fn prepare(
	context: &Context<'_>,
	room: &OwnedRoomOrAliasId,
	sweep: SweepArgs,
) -> Result<Prepared> {
	if !context.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.");
	}

	let room_id = context.services.alias.maybe_resolve(room).await?;
	let destinations = context
		.services
		.state_cache
		.room_servers(&room_id)
		.ready_filter(|server| {
			!sweep.no_loopback || !context.services.globals.server_is_ours(server)
		})
		.count()
		.await;

	if destinations > DESTINATION_LIMIT && !sweep.yes_i_want_to_do_this {
		return Err!(
			"Room has {destinations} feds destinations, exceeding the safety cap of \
			 {DESTINATION_LIMIT}. Pass the confirmation flag to continue."
		);
	}

	writeln!(context, "Querying {destinations} servers in {room_id}.\n").await?;

	let opts = Opts {
		width: Some(sweep.width),
		timeout: Some(Duration::from_secs(sweep.timeout)),
		sweep_deadline: Some(Duration::from_secs(sweep.budget)),
		exclude_self: sweep.no_loopback,
		record: Record::Observe,
	};

	Ok(Prepared { room_id, opts })
}

pub(super) fn fault_message(fault: &Fault) -> Cow<'static, str> {
	match fault {
		| Fault::Elapsed => Cow::Borrowed("request deadline exceeded"),
		| Fault::NotAttempted => Cow::Borrowed("sweep budget exhausted before dispatch"),
		| Fault::Error(error) => Cow::Owned(format!("{:?}: {}", error.kind(), error.message())),
	}
}

pub(super) fn markdown_cell(value: &str) -> Cow<'_, str> {
	if !value.chars().any(|character| {
		character.is_control() || matches!(character, '\\' | '|' | '\u{2028}' | '\u{2029}')
	}) {
		return Cow::Borrowed(value);
	}

	Cow::Owned(value.chars().fold(
		String::with_capacity(value.len()),
		|mut escaped, character| {
			match character {
				| '\\' => escaped.push_str("\\\\"),
				| '|' => escaped.push_str("\\|"),
				| '\u{2028}' | '\u{2029}' => escaped.push(' '),
				| character if character.is_control() => escaped.push(' '),
				| _ => escaped.push(character),
			}

			escaped
		},
	))
}

pub(super) fn sorted_event_id_difference(
	remote: &[OwnedEventId],
	local: &[OwnedEventId],
) -> Cow<'static, str> {
	let mut remote = remote.iter().peekable();
	let mut local = local.iter().peekable();
	let mut added = 0_usize;
	let mut missing = 0_usize;

	while let (Some(remote_id), Some(local_id)) = (remote.peek(), local.peek()) {
		match remote_id.cmp(local_id) {
			| Ordering::Less => {
				added = added.saturating_add(1);
				_ = remote.next();
			},
			| Ordering::Greater => {
				missing = missing.saturating_add(1);
				_ = local.next();
			},
			| Ordering::Equal => {
				_ = remote.next();
				_ = local.next();
			},
		}
	}

	added = added.saturating_add(remote.count());
	missing = missing.saturating_add(local.count());

	match (added, missing) {
		| (0, 0) => Cow::Borrowed("="),
		| _ => Cow::Owned(format!("+{added}/-{missing}")),
	}
}
