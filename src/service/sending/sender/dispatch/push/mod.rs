mod suppressed;

use futures::{FutureExt, TryFutureExt, future::try_join3};
use ruma::{OwnedUserId, api::error::ErrorKind};
use tuwunel_core::{
	Error, Event, debug, error,
	error::error_chain,
	extract_variant, implement,
	smallvec::SmallVec,
	utils::{BoolExt, IterStream, ReadyExt, stream::WidebandExt},
	warn,
};

use super::SendingResult;
use crate::{
	rooms::timeline::RawPduId,
	sending::{Destination, SendingEvent, Service},
};

type FailedIds = SmallVec<[RawPduId; 1]>;

// Notices in flight to one push gateway at once.
pub(super) const PUSH_WIDTH: usize = 4;

/// The PDUs a push transaction failed to deliver, with the first error.
///
/// Permanent errors are dropped before they reach here; the retained IDs keep
/// their active rows for the retry.
#[derive(Default)]
struct Failures {
	ids: FailedIds,
	error: Option<Error>,
}

#[implement(Service)]
#[tracing::instrument(
	name = "push",
	level = "info",
	skip(self, events),
	fields(
		events = events.len(),
	),
)]
pub(super) async fn send_events_dest_push(
	&self,
	user_id: OwnedUserId,
	pushkey: String,
	events: Vec<SendingEvent>,
) -> SendingResult {
	let has_pdu = events
		.iter()
		.any(|event| matches!(event, SendingEvent::Pdu(_)));

	let destination = || Destination::Push(user_id.clone(), pushkey.clone());
	let suppressed = self.pushing_suppressed(&user_id).map(Ok);
	let pusher = self
		.services
		.pusher
		.get_pusher(&user_id, &pushkey)
		.map(|result| match result {
			| Ok(pusher) => Ok(Some(pusher)),
			| Err(error) if error.is_not_found() => {
				error!(%user_id, %pushkey, "Pusher disappeared before delivery");

				Ok(None)
			},
			| Err(error) => Err((destination(), error)),
		});

	let ruleset = has_pdu
		.then_async(|| self.services.pusher.ruleset(&user_id))
		.map(Ok);

	let (pusher, ruleset, suppressed) = try_join3(pusher, ruleset, suppressed).await?;

	let Some(pusher) = pusher else {
		return Ok(Destination::Push(user_id, pushkey));
	};

	// Reconciliation, not an alert: a suppressed drop strands a stale badge.
	if events.contains(&SendingEvent::BadgeRefresh) {
		let result = self
			.services
			.pusher
			.send_badge_notice(&user_id, &pusher)
			.await;

		match result {
			| Ok(()) => (),
			| Err(error) if is_permanent_error(&error) => warn!(
				%user_id,
				%pushkey,
				chain = %error_chain(&error),
				"Dropping a badge push with a permanent local error",
			),
			| Err(error) => return Err((destination(), error)),
		}
	}

	if suppressed {
		let queued = self
			.enqueue_suppressed_push_events(&user_id, &pushkey, &events)
			.await;

		debug!(
			%user_id,
			%pushkey,
			queued,
			events = events.len(),
			"Push suppressed; queued events"
		);

		return Ok(Destination::Push(user_id, pushkey));
	}

	self.schedule_flush_suppressed_for_pushkey(
		user_id.clone(),
		pushkey.clone(),
		"non-suppressed push",
	);

	let Some(ruleset) = ruleset else {
		return Ok(Destination::Push(user_id, pushkey));
	};

	let pdu_ids = || {
		events
			.iter()
			.filter_map(|event| extract_variant!(event, SendingEvent::Pdu))
	};

	let failures = pdu_ids()
		.stream()
		.wide_filter_map(async |pdu_id| {
			self.services
				.timeline
				.get_pdu_from_id(pdu_id)
				.map_ok(|pdu| (*pdu_id, pdu))
				.await
				.ok()
		})
		.ready_filter(|(_, pdu)| !pdu.is_redacted())
		.widen_then(Some(PUSH_WIDTH), async |(pdu_id, pdu)| {
			let result = self
				.services
				.pusher
				.send_push_notice(&user_id, &pusher, &ruleset, &pdu)
				.await;

			(pdu_id, result)
		})
		.ready_fold(Failures::default(), |failures, (pdu_id, result)| match result {
			| Ok(()) => failures,
			| Err(error) if is_permanent_error(&error) => {
				warn!(
					%user_id,
					%pushkey,
					?pdu_id,
					chain = %error_chain(&error),
					"Dropping a push with a permanent local error",
				);

				failures
			},
			| Err(error) => failures.retain(pdu_id, error),
		})
		.await;

	let Failures { ids, error: Some(error) } = failures else {
		return Ok(Destination::Push(user_id, pushkey));
	};

	let destination = Destination::Push(user_id, pushkey);

	pdu_ids()
		.filter(|pdu_id| !ids.contains(*pdu_id))
		.for_each(|pdu_id| {
			self.db
				.delete_active_request(&destination.event_key(pdu_id));
		});

	Err((destination, error))
}

impl Failures {
	fn retain(mut self, pdu_id: RawPduId, error: Error) -> Self {
		self.ids.push(pdu_id);
		self.error = self.error.or(Some(error));

		self
	}
}

#[inline]
fn is_permanent_error(error: &Error) -> bool {
	matches!(error, Error::Request(ErrorKind::InvalidParam, ..))
}
