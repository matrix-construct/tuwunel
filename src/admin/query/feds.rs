mod event;
mod head;
mod ping;
mod state;
#[cfg(test)]
mod tests;
mod version;

use std::{
	borrow::Cow,
	cmp::Ordering,
	collections::BTreeMap,
	fmt::{Result as FmtResult, Write as _},
	num::NonZeroUsize,
	time::Duration,
};

use clap::{ArgAction, Args, Subcommand, ValueEnum};
use futures::StreamExt;
use ruma::{OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId};
use tuwunel_core::{
	Err, Result, implement,
	utils::{stream::ReadyExt, time::Elapsed},
};
use tuwunel_service::federation::{
	PeerBackoff,
	feds::{Fault, Opts, Outcome, Record},
};

use self::version::Field;
use crate::{Context, admin_command_dispatch};

pub(super) type Backoffs = BTreeMap<OwnedServerName, PeerBackoff>;

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

		/// Select metadata fields used to group and display versions.
		#[arg(long = "field", value_enum)]
		fields: Vec<Field>,

		/// List servers whose request did not produce an error.
		#[arg(long, group = "version_list")]
		list: bool,

		/// List every server.
		#[arg(long, group = "version_list")]
		list_all: bool,

		/// List servers whose request produced an error.
		#[arg(long, group = "version_list")]
		list_errors: bool,

		/// Order the listed servers by this column.
		#[arg(long, value_enum, default_value_t, requires = "version_list")]
		sort: Sort,

		#[command(flatten)]
		sweep: SweepArgs,
	},

	/// Measure request latency to participating servers.
	///
	/// Reports latency distribution statistics beside the peer-status record
	/// held for each destination.
	Ping {
		room: OwnedRoomOrAliasId,

		/// List servers whose request did not produce an error.
		#[arg(long, group = "ping_list")]
		list: bool,

		/// List every server.
		#[arg(long, group = "ping_list")]
		list_all: bool,

		/// List servers whose request produced an error.
		#[arg(long, group = "ping_list")]
		list_errors: bool,

		/// Order the listed servers by this column.
		#[arg(long, value_enum, default_value_t, requires = "ping_list")]
		sort: Sort,

		#[command(flatten)]
		sweep: SweepArgs,
	},

	/// Compare copies of one event reported by participating servers.
	///
	/// Reports transport latency and enabled validation results for every
	/// destination.
	Event {
		event_id: OwnedEventId,

		/// Select the room containing the event.
		///
		/// The local PDU supplies the room when this argument is omitted.
		room: Option<OwnedRoomOrAliasId>,

		/// Control event content-hash verification.
		///
		/// Verification is enabled by default.
		#[arg(long, default_value_t = true, action = ArgAction::Set)]
		verify_hash: bool,

		/// Control verification of signatures required by the room version.
		///
		/// Verification is enabled by default.
		#[arg(long, default_value_t = true, action = ArgAction::Set)]
		verify_signature: bool,

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
	#[arg(long)]
	width: Option<NonZeroUsize>,

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

/// Column ordering the detail listing.
///
/// Rows equal under the chosen column keep their origin order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum Sort {
	/// Server name.
	#[default]
	Origin,

	/// Request latency, fastest first.
	Elapsed,

	/// Failure message.
	Fault,
}

#[derive(Clone, Copy)]
pub(super) enum ListMode {
	None,
	Successes,
	All,
	Errors,
}

pub(super) struct Prepared {
	pub(super) room_id: OwnedRoomId,
	pub(super) opts: Opts,
}

pub(super) async fn prepare(
	context: &Context<'_>,
	room: &OwnedRoomOrAliasId,
	sweep: SweepArgs,
	default_width: NonZeroUsize,
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

	let destination_limit = context
		.services
		.server
		.config
		.feds_destination_limit;

	if destinations > destination_limit && !sweep.yes_i_want_to_do_this {
		return Err!(
			"Room has {destinations} feds destinations, exceeding the safety cap of \
			 {destination_limit}. Pass the confirmation flag to continue."
		);
	}

	writeln!(context, "Querying {destinations} servers in {room_id}.\n").await?;

	let opts = Opts {
		width: Some(sweep.width.unwrap_or(default_width)),
		timeout: Some(Duration::from_secs(sweep.timeout)),
		sweep_deadline: Some(Duration::from_secs(sweep.budget)),
		exclude_self: sweep.no_loopback,
		record: Record::Observe,
	};

	Ok(Prepared { room_id, opts })
}

#[implement(ListMode)]
pub(super) fn new(list: bool, list_all: bool, list_errors: bool) -> Self {
	match (list, list_all, list_errors) {
		| (true, false, false) => Self::Successes,
		| (false, true, false) => Self::All,
		| (false, false, true) => Self::Errors,
		| _ => Self::None,
	}
}

#[implement(ListMode)]
pub(super) fn includes<T>(self, outcome: &Outcome<T>) -> bool {
	match self {
		| Self::None => false,
		| Self::Successes => outcome.result.is_ok(),
		| Self::All => true,
		| Self::Errors => outcome.result.is_err(),
	}
}

/// Splits the room's destinations into dispatchable origins and held origins.
///
/// An origin still inside its peer backoff at `now` becomes a `Fault::Backoff`
/// outcome with zero elapsed time instead of a destination.
pub(super) async fn partition_backoffs<T>(
	context: &Context<'_>,
	prepared: &Prepared,
	backoffs: &Backoffs,
	now: u64,
) -> (Vec<OwnedServerName>, Vec<Outcome<T>>) {
	context
		.services
		.state_cache
		.room_servers(&prepared.room_id)
		.ready_filter(|server| {
			!prepared.opts.exclude_self || !context.services.globals.server_is_ours(server)
		})
		.map(ToOwned::to_owned)
		.ready_fold((Vec::new(), Vec::new()), |(mut eligible, mut outcomes), origin| {
			match backoffs
				.get(&origin)
				.and_then(|backoff| backoff_fault(backoff, now))
			{
				| None => eligible.push(origin),
				| Some(fault) => outcomes.push(Outcome {
					origin,
					elapsed: Duration::ZERO,
					result: Err(fault),
				}),
			}

			(eligible, outcomes)
		})
		.await
}

/// Describes a peer's backoff as a fault while it has not expired at `now`.
///
/// The age spans the oldest surviving failure bucket and the retry is the
/// remaining delay.
pub(super) fn backoff_fault(backoff: &PeerBackoff, now: u64) -> Option<Fault> {
	retry_after(backoff, now).map(|retry| Fault::Backoff {
		class: backoff.class,
		age: Duration::from_secs(now.saturating_sub(backoff.oldest_secs)),
		retry,
	})
}

/// Computes the remaining delay before the peer becomes eligible.
///
/// The delay is measured from the newest failure; an expired backoff yields
/// `None`.
pub(super) fn retry_after(backoff: &PeerBackoff, now: u64) -> Option<Duration> {
	let retry_at = backoff
		.anchor_secs
		.saturating_add(backoff.delay_secs);

	retry_at
		.gt(&now)
		.then(|| Duration::from_secs(retry_at.saturating_sub(now)))
}

pub(super) fn sorted<T>(
	mut outcomes: Vec<Outcome<T>>,
	sort: Sort,
	fault_key: impl Fn(&Outcome<T>) -> Cow<'static, str>,
) -> Vec<Outcome<T>> {
	// Both secondary sorts are stable, so origin order remains the tie-breaker.
	outcomes.sort_by(|left, right| left.origin.cmp(&right.origin));
	match sort {
		| Sort::Origin => {},
		| Sort::Elapsed => outcomes.sort_by_key(|outcome| outcome.elapsed),
		| Sort::Fault => outcomes.sort_by_cached_key(fault_key),
	}

	outcomes
}

pub(super) fn fault_message(fault: &Fault) -> Cow<'static, str> {
	match fault {
		| Fault::Elapsed => Cow::Borrowed("request deadline exceeded"),
		| Fault::NotAttempted => Cow::Borrowed("sweep budget exhausted before dispatch"),
		| Fault::Backoff { class, age, retry } => Cow::Owned(format!(
			"peer backoff ({class:?}, age {}, retry {})",
			Elapsed::from(*age),
			Elapsed::from(*retry),
		)),
		| Fault::Error(error) => Cow::Owned(format!("{:?}: {}", error.kind(), error.message())),
	}
}

pub(super) fn count_results<T>(outcomes: &[Outcome<T>]) -> usize {
	outcomes
		.iter()
		.filter(|outcome| outcome.result.is_ok())
		.count()
}

pub(super) fn render_totals(
	output: &mut String,
	results: usize,
	duration: Duration,
) -> FmtResult {
	let noun = if results == 1 { "result" } else { "results" };

	writeln!(output, "\n{results} {noun} in {}.", Elapsed::from(duration))
}

/// Writes the elapsed cell of a listing row.
///
/// A destination that was never dispatched has no latency and takes a blank
/// cell.
pub(super) fn write_elapsed_cell<T>(output: &mut String, outcome: &Outcome<T>) -> FmtResult {
	if matches!(&outcome.result, Err(Fault::NotAttempted | Fault::Backoff { .. })) {
		write!(output, " |")
	} else {
		write!(output, " {} |", Elapsed::from(outcome.elapsed))
	}
}

pub(super) fn write_cell(output: &mut String, value: &str) -> FmtResult {
	if value.is_empty() {
		write!(output, " |")
	} else {
		write!(output, " {value} |")
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
