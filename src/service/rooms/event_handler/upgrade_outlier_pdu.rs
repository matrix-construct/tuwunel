use std::{borrow::Borrow, collections::HashMap, iter::once, sync::Arc, time::Instant};

use futures::{FutureExt, StreamExt};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, RoomId, RoomVersionId, ServerName,
	events::StateEventType, room_version_rules::RoomVersionRules,
};
use tuwunel_core::{
	Result, debug, debug_info, debug_warn, err, implement, is_equal_to,
	matrix::{Event, EventTypeExt, PduEvent, StateKey, pdu::check_rules, room_version},
	trace,
	utils::{
		BoolExt,
		stream::{BroadbandExt, ReadyExt},
	},
	warn,
};

use super::{
	backoff::{Context, Disposition, UPGRADE_RETRY},
	policy_server::PolicyCheck,
	state_local_build::WalkMode,
};
use crate::rooms::{
	state::{RoomMutexGuard, Trigger, prune_goal},
	state_compressor::{CompressedState, HashSetCompressStateEvent},
	state_res::{AuthCheckOutcome, auth_check},
	timeline::RawPduId,
};

#[cfg(test)]
mod tests;

/// How the state at the incoming event was obtained, deciding the memo write
/// at the upgrade site.
#[derive(Clone, Copy)]
enum ResolvedVia {
	Derived,
	Memo,
	Local,
	Fetch,
}

/// Outcome of re-examining an event that already carries a soft-fail marker.
#[derive(Clone, Copy)]
enum Standing {
	/// Evaluate the event. `true` when a re-check already cleared a lapsed
	/// marker, so the soft-fail computation is settled.
	Evaluate(bool),

	/// The standing verdict holds; leave the event withheld.
	Withheld,
}

#[implement(super::Service)]
#[tracing::instrument(
	name = "upgrade",
	level = "debug",
	ret(level = "debug"),
	skip_all,
	fields(lev = %recursion_level)
)]
#[expect(clippy::too_many_arguments)]
pub(super) async fn upgrade_outlier_to_timeline_pdu(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	incoming_pdu: PduEvent,
	mut pdu_json: CanonicalJsonObject,
	room_version: &RoomVersionId,
	recursion_level: usize,
	create_event_id: &EventId,
) -> Result<Option<(RawPduId, bool)>> {
	// Skip the PDU if we already have it as a timeline event
	if let Ok(pdu_id) = self
		.services
		.timeline
		.get_pdu_id(incoming_pdu.event_id())
		.await
	{
		debug!(?pdu_id, "Exists.");
		return Ok(Some((pdu_id, false)));
	}

	trace!("Upgrading to timeline pdu");

	let timer = Instant::now();
	let room_rules = room_version::rules(room_version)?;

	trace!(format = ?room_rules.event_format, "Checking format");
	check_rules(&pdu_json, &room_rules.event_format)?;

	let Standing::Evaluate(cleared) = self
		.soft_fail_standing(&incoming_pdu, &room_rules, &mut pdu_json)
		.await?
	else {
		return Ok(None);
	};

	let (state_at_incoming_event, resolved_via) = self
		.resolve_state_at_incoming_event(
			origin,
			room_id,
			&incoming_pdu,
			room_version,
			recursion_level,
			create_event_id,
		)
		.await?;

	self.auth_check_outlier_pdu(room_id, &incoming_pdu, &room_rules, &state_at_incoming_event)
		.await?;

	let soft_fail_pre = !cleared
		&& self
			.compute_soft_fail(&incoming_pdu, &room_rules, &mut pdu_json)
			.await?;

	// 13. Use state resolution to find new room state
	// We start looking at current room state now, so lets lock the room
	trace!("Locking the room");
	let state_lock = self.services.state.mutex.lock(room_id).await;

	// 14. Check if the event passes auth based on the current room state.
	let soft_fail_current_state = !self
		.current_state_auth_passes(room_id, &incoming_pdu, &room_rules)
		.await?;

	let soft_fail = soft_fail_pre || soft_fail_current_state;

	let mut extremities = self
		.compute_remaining_extremities(room_id, &incoming_pdu)
		.await;

	let config = &self.services.server.config;
	let max = config.forward_extremities_max;
	let len = extremities
		.len()
		.saturating_add(usize::from(!soft_fail));

	if max > 0 && len > max {
		let goal = prune_goal(
			len,
			max,
			config.forward_extremities_emergency_max,
			config.forward_extremities_prune_batch,
		);

		let summary = self
			.services
			.state
			.prune_forward_extremities(room_id, &mut extremities, goal, Trigger::Receive)
			.await;

		debug!(?summary, "Pruned forward extremities over the cap.");
	}

	trace!("Compressing state...");
	let state_ids_compressed: Arc<CompressedState> = self
		.services
		.state_compressor
		.compress_state_events(
			state_at_incoming_event
				.iter()
				.map(|(ssk, eid)| (ssk, eid.borrow())),
		)
		.collect()
		.map(Arc::new)
		.await;

	// Memoize only complete local or fetched state after positional auth succeeds;
	// soft-failed events cannot establish reusable state.
	if matches!(resolved_via, ResolvedVia::Local | ResolvedVia::Fetch) && !soft_fail {
		self.cache_resolved_state(room_id, incoming_pdu.event_id(), state_ids_compressed.clone())
			.await;
	}

	// A soft-failed event is not a forward extremity, so it never drives the
	// room's current state; only an accepted state event resolves forward.
	if incoming_pdu.state_key().is_some() && !soft_fail {
		self.resolve_and_force_state_after(
			room_id,
			room_version,
			&incoming_pdu,
			&state_at_incoming_event,
			&state_lock,
		)
		.boxed()
		.await?;
	}

	// We use the `state_at_event` instead of `state_after` so we accurately
	// represent the state for this event.
	trace!("Appending pdu to timeline");

	// Incoming event will be referenced in prev_events unless soft-failed.
	let incoming_extremity = once(incoming_pdu.event_id()).filter(|_| !soft_fail);

	let extremities = extremities
		.iter()
		.map(Borrow::borrow)
		.chain(incoming_extremity);

	let pdu_id = self
		.services
		.timeline
		.append_incoming_pdu(
			&incoming_pdu,
			pdu_json,
			extremities,
			state_ids_compressed,
			soft_fail,
			&state_lock,
		)
		.await?;

	debug_assert!(
		pdu_id.is_some() || soft_fail,
		"Ok(None) returned by timeline for soft-failed PDU's"
	);

	if soft_fail {
		self.services
			.pdu_metadata
			.mark_event_soft_failed(incoming_pdu.event_id());

		self.record_outcome(Context::Upgrade, incoming_pdu.event_id(), Disposition::Transient);

		drop(state_lock);
		warn!(
			event_id = %incoming_pdu.event_id(),
			redact_or_policy = soft_fail_pre,
			current_state = soft_fail_current_state,
			elapsed = ?timer.elapsed(),
			"Event was soft failed.",
		);

		return Ok(None);
	}

	drop(state_lock);

	if cleared {
		self.services
			.pdu_metadata
			.clear_event_soft_failed(incoming_pdu.event_id());

		self.record_success(Context::Upgrade, incoming_pdu.event_id())
			.await;
	}

	debug_info!(
		elapsed = ?timer.elapsed(),
		"Accepted",
	);

	Ok(pdu_id.zip(Some(true)))
}

#[implement(super::Service)]
#[tracing::instrument(
	name = "auth_current",
	level = "debug",
	skip_all,
	fields(%room_id, event_id = %incoming_pdu.event_id()),
)]
async fn current_state_auth_passes(
	&self,
	room_id: &RoomId,
	incoming_pdu: &PduEvent,
	room_rules: &RoomVersionRules,
) -> Result<bool> {
	trace!("Gathering current-state auth events.");
	let auth_events = self
		.services
		.state
		.get_auth_events(
			room_id,
			incoming_pdu.kind(),
			incoming_pdu.sender(),
			incoming_pdu.state_key(),
			incoming_pdu.content(),
			&room_rules.authorization,
			true,
		)
		.await;

	trace!("Performing current-state auth check.");
	let outcome = current_state_auth_outcome(auth_events, async move |auth_events| {
		let state_fetch = async |k: StateEventType, s: StateKey| {
			auth_events
				.get(&k.with_state_key(s.as_str()))
				.map(ToOwned::to_owned)
				.ok_or_else(|| err!(Request(NotFound("state event not found"))))
		};

		let event_fetch = async |event_id: OwnedEventId| self.event_fetch(&event_id).await;

		auth_check(room_rules, incoming_pdu, &event_fetch, &state_fetch).await
	})
	.await;

	match outcome {
		| Ok(AuthCheckOutcome::Allow) => Ok(true),
		| Ok(AuthCheckOutcome::Deny(error)) => {
			warn!(
				auth_leg = "current_state",
				event_id = %incoming_pdu.event_id(),
				%room_id,
				%error,
				"Current-state auth check failed; soft-failing event.",
			);

			Ok(false)
		},
		| Err(error) => {
			warn!(
				auth_leg = "current_state",
				event_id = %incoming_pdu.event_id(),
				%room_id,
				%error,
				"Current-state auth check could not be evaluated.",
			);

			Err(error)
		},
	}
}

async fn current_state_auth_outcome<AuthEvents, Check, CheckFuture>(
	auth_events: Result<AuthEvents>,
	check: Check,
) -> Result<AuthCheckOutcome>
where
	Check: FnOnce(AuthEvents) -> CheckFuture,
	CheckFuture: Future<Output = Result<AuthCheckOutcome>>,
{
	check(auth_events?).await
}

/// Re-examines an event that already carries a soft-fail marker.
///
/// The marker is a standing verdict rather than a permanent rejection, so it
/// lapses on the upgrade backoff and the event is weighed again. Asking before
/// state resolution keeps a still-refused event cheap to decline, since a
/// cached policy answer needs no round trip.
#[implement(super::Service)]
async fn soft_fail_standing(
	&self,
	incoming_pdu: &PduEvent,
	room_rules: &RoomVersionRules,
	pdu_json: &mut CanonicalJsonObject,
) -> Result<Standing> {
	let event_id = incoming_pdu.event_id();

	if !self
		.services
		.pdu_metadata
		.is_event_soft_failed(event_id)
		.await
	{
		return Ok(Standing::Evaluate(false));
	}

	if self
		.is_suppressed(Context::Upgrade, event_id, UPGRADE_RETRY)
		.await
		.is_deny()
	{
		debug!(%event_id, "Soft failed; deferring re-evaluation.");
		return Ok(Standing::Withheld);
	}

	if self
		.compute_soft_fail(incoming_pdu, room_rules, pdu_json)
		.await?
	{
		self.record_outcome(Context::Upgrade, event_id, Disposition::Transient);

		debug!(%event_id, "Still soft failed.");
		return Ok(Standing::Withheld);
	}

	Ok(Standing::Evaluate(true))
}

#[implement(super::Service)]
async fn resolve_state_at_incoming_event(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	incoming_pdu: &PduEvent,
	room_version: &RoomVersionId,
	recursion_level: usize,
	create_event_id: &EventId,
) -> Result<(HashMap<u64, OwnedEventId>, ResolvedVia)> {
	// 10. Fetch missing state and auth chain events by calling /state_ids at
	//     backwards extremities doing all the checks in this list starting at 1.
	//     These are not timeline events.
	trace!("Resolving state at event");

	let state_at_incoming_event = if incoming_pdu.prev_events().count() == 1 {
		self.state_at_incoming_degree_one(incoming_pdu)
			.await?
	} else {
		self.state_at_incoming_resolved(incoming_pdu, room_id, room_version)
			.boxed()
			.await?
	};

	if let Some(state) = state_at_incoming_event {
		return Ok((state, ResolvedVia::Derived));
	}

	let config = &self.services.server.config;
	let enabled = config.resolve_state_locally && config.resolve_state_locally_max > 0;

	if enabled {
		match self
			.cached_resolved_state(incoming_pdu.event_id())
			.await
		{
			| Ok(Some(state)) => return Ok((state, ResolvedVia::Memo)),
			| Ok(None) => (),
			| Err(error) => debug_warn!(
				event_id = %incoming_pdu.event_id(),
				%error,
				"Failed to load resolved state memo.",
			),
		}
	}

	let mode = if config.resolve_state_locally_shadow {
		WalkMode::Shadow
	} else {
		WalkMode::Active
	};

	let local = enabled
		.then_async(|| {
			self.state_at_incoming_local(
				room_id,
				incoming_pdu,
				room_version,
				create_event_id,
				mode,
			)
			.boxed()
		})
		.await
		.transpose()?
		.flatten();

	let shadow = match (mode, local) {
		| (WalkMode::Active, Some(state)) => return Ok((state, ResolvedVia::Local)),
		| (WalkMode::Shadow, local) => local,
		| (WalkMode::Active, None) => None,
	};

	let state = self
		.fetch_state(
			origin,
			room_id,
			incoming_pdu.event_id(),
			room_version,
			recursion_level,
			create_event_id,
		)
		.boxed()
		.await?
		.expect("fetch_state always resolves state to some");

	if let Some(local) = shadow {
		self.compare_shadow(room_id, incoming_pdu.event_id(), &local, &state)
			.await;
	}

	Ok((state, ResolvedVia::Fetch))
}

#[implement(super::Service)]
async fn auth_check_outlier_pdu(
	&self,
	room_id: &RoomId,
	incoming_pdu: &PduEvent,
	room_rules: &RoomVersionRules,
	state_at_incoming_event: &HashMap<u64, OwnedEventId>,
) -> Result {
	// Every event holding a `shorteventid_shortstatehash` row passed spec check 5
	// (auth against the state at its own position) as a hard reject; soft failure
	// (spec check 6) still writes the row, so soft-failed events are valid fold
	// inputs while positionally rejected events never gain a row.

	let state_fetch = async |k: StateEventType, s: StateKey| {
		let shortstatekey = self
			.services
			.short
			.get_shortstatekey(&k, s.as_str())
			.await?;

		let event_id = state_at_incoming_event
			.get(&shortstatekey)
			.ok_or_else(|| {
				err!(Request(NotFound(
					"shortstatekey {shortstatekey:?} not found for ({k:?},{s:?})"
				)))
			})?;

		self.services
			.timeline
			.get_pdu(event_id)
			.await
			.map_err(|error| {
				if error.is_not_found() {
					err!(Database("State map references missing event {event_id}."))
				} else {
					error
				}
			})
	};

	let event_fetch = async |event_id: OwnedEventId| self.event_fetch(&event_id).await;

	trace!("Performing positional auth check.");
	auth_check(room_rules, incoming_pdu, &event_fetch, &state_fetch)
		.await
		.and_then(AuthCheckOutcome::into_result)
		.inspect_err(|error| {
			warn!(
				auth_leg = "positional",
				event_id = %incoming_pdu.event_id(),
				%room_id,
				%error,
				"Positional auth check failed.",
			);
		})
}

#[implement(super::Service)]
async fn compute_soft_fail(
	&self,
	incoming_pdu: &PduEvent,
	room_rules: &RoomVersionRules,
	pdu_json: &mut CanonicalJsonObject,
) -> Result<bool> {
	// Soft fail check before doing state res
	trace!("Performing soft-fail check");
	let soft_fail_redact = match incoming_pdu.redacts_id(room_rules) {
		| None => false,
		| Some(redact_id) =>
			!self
				.services
				.state_accessor
				.user_can_redact(&redact_id, incoming_pdu.sender(), incoming_pdu.room_id(), true)
				.await?,
	};

	// MSC4284: soft-fail when the policy server rejects the event.
	Ok(soft_fail_redact
		|| matches!(
			self.verify_or_fetch_inbound_policy_signature(pdu_json, incoming_pdu)
				.await,
			PolicyCheck::Invalid,
		))
}

#[implement(super::Service)]
async fn compute_remaining_extremities(
	&self,
	room_id: &RoomId,
	incoming_pdu: &PduEvent,
) -> Vec<OwnedEventId> {
	// Now we calculate the set of extremities this room has after the incoming
	// event has been applied. We start with the previous extremities (aka leaves)
	trace!("Calculating extremities");
	let extremities: Vec<_> = self
		.services
		.state
		.get_forward_extremities(room_id)
		.ready_filter(|&event_id| {
			// Remove any that are referenced by this incoming event's prev_events
			!incoming_pdu
				.prev_events()
				.any(is_equal_to!(event_id))
		})
		.map(ToOwned::to_owned)
		.broad_filter_map(async |event_id| {
			// Only keep those extremities were not referenced yet
			self.services
				.pdu_metadata
				.is_event_referenced(room_id, &event_id)
				.await
				.eq(&false)
				.then_some(event_id)
		})
		.collect()
		.await;

	debug!(
		retained = extremities.len(),
		prev_events = incoming_pdu.prev_events().count(),
		"Retained extremities checked against prev_events.",
	);

	extremities
}

#[implement(super::Service)]
async fn resolve_and_force_state_after(
	&self,
	room_id: &RoomId,
	room_version: &RoomVersionId,
	incoming_pdu: &PduEvent,
	state_at_incoming_event: &HashMap<u64, OwnedEventId>,
	state_lock: &RoomMutexGuard,
) -> Result {
	// We also add state after incoming event to the fork states
	let mut state_after = state_at_incoming_event.clone();
	if let Some(state_key) = incoming_pdu.state_key() {
		let event_id = incoming_pdu.event_id();
		let event_type = incoming_pdu.kind();
		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&event_type.to_string().into(), state_key)
			.await;

		state_after.insert(shortstatekey, event_id.to_owned());
		// Now it's the state after the event.
		debug!(
			?event_id,
			?event_type,
			?state_key,
			?shortstatekey,
			state_after = state_after.len(),
			"Adding event to state."
		);
	}

	trace!("Resolving new room state.");
	let new_room_state = self
		.resolve_state(room_id, room_version, state_after)
		.boxed()
		.await?;

	// Set the new room state to the resolved state
	trace!("Saving resolved state.");
	let HashSetCompressStateEvent { shortstatehash, added, removed } = self
		.services
		.state_compressor
		.save_state(room_id, new_room_state)
		.await?;

	debug!(
		?shortstatehash,
		added = added.len(),
		removed = removed.len(),
		"Forcing new room state."
	);
	self.services
		.state
		.force_state(room_id, shortstatehash, added, removed, state_lock)
		.await?;

	Ok(())
}
