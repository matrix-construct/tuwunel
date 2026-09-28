use std::{
	iter::once,
	pin::pin,
	sync::atomic::{AtomicBool, Ordering},
};

use futures::{TryStreamExt, future::ready};
use tuwunel_core::{
	Err, Result, err,
	itertools::Itertools,
	matrix::{PduEvent, pdu::into_outgoing_federation},
	pdu::PduBuilder,
	ruma::{
		EventId, OwnedEventId, RoomId, RoomVersionId, UserId,
		events::{
			StateEventType,
			room::member::{MembershipState, RoomMemberEventContent},
		},
	},
	utils::{BoolExt, result::NotFound},
};
use tuwunel_database::serialize_key;
use tuwunel_service::Services;

use super::helpers::{
	CacheHandling, ExpectedWalkOutcome, HeldFork, PduFailure, append_message, assert_accepts,
	assert_fetches, assert_no_memo, assert_unevaluable, corrupt_timeline_pdu, create_room,
	create_room_version, held_fork, held_state_fork, set_forward_extremity, sign_message,
	sign_outlier_message, suppress_upgrade,
};

#[derive(Clone, Copy)]
pub(super) enum AncestorFailure {
	Missing,
	InteriorFork,
	V12InteriorFork,
}

pub(super) async fn ancestor_failure(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
	failure: AncestorFailure,
) -> Result {
	let interior =
		matches!(failure, AncestorFailure::InteriorFork | AncestorFailure::V12InteriorFork);

	let room_id = match failure {
		| AncestorFailure::V12InteriorFork =>
			create_room_version(services, base, token, &RoomVersionId::V12).await?,
		| _ => create_room(services, base, token).await?,
	};

	let ancestor = verified_replaced_membership_ancestor(services, &room_id, user_id).await?;
	let (left, right, fork, fork_json) = held_state_fork(services, user_id, &room_id).await?;
	let (top, top_json) = if interior {
		set_forward_extremity(services, &room_id, fork.event_id.as_ref()).await;

		sign_outlier_message(services, user_id, &room_id, "sentinel top").await?
	} else {
		(fork.clone(), fork_json)
	};

	let cached_auth_chain = interior
		.then_async(|| prime_local_auth_chain(services, top.event_id.as_ref()))
		.await
		.transpose()?;

	let cache_handling = cached_auth_chain
		.as_ref()
		.map_or(CacheHandling::Clear, |_| CacheHandling::Preserve);

	corrupt_timeline_pdu(services, &ancestor, PduFailure::Missing, cache_handling).await?;

	if let Some(key) = cached_auth_chain {
		services.db["authchainkey_authchain"]
			.exists(&key)
			.await
			.map_err(|error| err!("warmed auth-chain row was not preserved: {error}"))?;
	}

	suppress_upgrade(services, left.event_id.as_ref())?;
	suppress_upgrade(services, right.event_id.as_ref())?;

	if interior {
		suppress_upgrade(services, fork.event_id.as_ref())?;
	}

	let context = match failure {
		| AncestorFailure::Missing => "missing auth ancestor",
		| AncestorFailure::InteriorFork => "interior fork sentinel",
		| AncestorFailure::V12InteriorFork => "v12 resolver failure",
	};

	assert_unevaluable(services, top.event_id.as_ref(), context).await?;

	if interior {
		assert_no_memo(services, fork.event_id.as_ref()).await?;
	}

	assert_fetches(services, &room_id, &top, top_json, ExpectedWalkOutcome::Unevaluable, context)
		.await?;

	Ok(())
}

pub(super) async fn corrupt_chain_cache_rebuilds(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let membership = member_event(services, &room_id, user_id).await?;
	let room_version = services.state.get_room_version(&room_id).await?;
	let shorteventid = services
		.short
		.get_shorteventid(membership.event_id.as_ref())
		.await?;

	let key = serialize_key([shorteventid].as_slice())?;
	let sorted_chain = async |complete: &AtomicBool| -> Result<Vec<OwnedEventId>> {
		services
			.auth_chain
			.event_ids_iter_strict(
				&room_id,
				&room_version,
				once(membership.event_id.as_ref()),
				complete,
			)
			.try_collect::<Vec<_>>()
			.await
			.map(|event_ids| event_ids.into_iter().sorted_unstable().collect())
	};

	services.clear_cache().await;

	let first_complete = AtomicBool::new(true);
	let expected = sorted_chain(&first_complete).await?;

	assert!(first_complete.load(Ordering::Relaxed), "initial auth chain was incomplete");
	assert!(!expected.is_empty(), "auth-chain cache fixture has an empty chain");

	let cache = services.db.get("authchainkey_authchain")?;

	cache
		.exists(&key)
		.await
		.map_err(|error| err!("initial auth-chain cache row was not written: {error}"))?;

	cache.insert(&key, b"!");

	let complete = AtomicBool::new(true);
	let rebuilt = sorted_chain(&complete).await?;

	assert_eq!(rebuilt, expected, "malformed auth-chain cache did not rebuild");
	assert!(complete.load(Ordering::Relaxed), "cache rebuild tripped completeness");

	let row = cache.get(&key).await?;

	assert!(
		row.len().is_multiple_of(size_of::<u64>()),
		"rebuilt auth-chain cache remains malformed"
	);

	assert_ne!(&*row, b"!", "malformed auth-chain cache was not replaced");

	Ok(())
}

pub(super) async fn unpolled_chain_stays_clear(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let ancestor = verified_replaced_membership_ancestor(services, &room_id, user_id).await?;
	let (left, right, top, top_json) = held_message_fork(services, user_id, &room_id).await?;

	corrupt_timeline_pdu(services, &ancestor, PduFailure::Missing, CacheHandling::Clear).await?;
	suppress_upgrade(services, left.event_id.as_ref())?;
	suppress_upgrade(services, right.event_id.as_ref())?;

	let report = services
		.event_handler
		.local_state_report(top.event_id.as_ref())
		.await?;

	assert_eq!(report.forks, 1, "unpolled fixture missed its fork");
	assert_eq!(report.gate_drops, 0, "unpolled chain became a denial");
	assert_eq!(report.fallback, None, "unpolled chain tripped the sentinel");
	assert!(report.state_len.is_some(), "unpolled chain produced no state");
	assert_accepts(services, &room_id, &top, top_json, "unpolled chain").await
}

pub(super) async fn current_state_auth_failure(
	services: &Services,
	base: &str,
	token: &str,
	user_id: &UserId,
) -> Result {
	let room_id = create_room(services, base, token).await?;
	let incoming_id =
		append_message(services, user_id, &room_id, "current-state dependency failure").await?;

	let incoming = services.timeline.get_pdu(&incoming_id).await?;
	let incoming_json = services
		.timeline
		.get_pdu_json(&incoming_id)
		.await?;

	verified_replaced_membership_ancestor(services, &room_id, user_id).await?;

	let current_membership = member_event(services, &room_id, user_id).await?;

	corrupt_timeline_pdu(
		services,
		current_membership.event_id.as_ref(),
		PduFailure::MalformedMembership,
		CacheHandling::Clear,
	)
	.await?;

	services.db["eventid_pduid"].remove(incoming.event_id.as_bytes());
	services
		.timeline
		.add_pdu_outlier(&incoming.event_id, &incoming_json);

	let room_version = services.state.get_room_version(&room_id).await?;
	let incoming_json = into_outgoing_federation(incoming_json, &room_version);

	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			&room_id,
			incoming.event_id.as_ref(),
			incoming_json,
			true,
		)
		.await;

	assert!(
		result.is_err(),
		"current-state dependency failure returned {result:?}; incoming {}; dependency {}",
		incoming.event_id,
		current_membership.event_id,
	);

	let unmarked = services
		.pdu_metadata
		.is_event_soft_failed(incoming.event_id.as_ref())
		.await
		.is_false();

	assert!(unmarked, "current-state dependency failure persisted a soft-fail marker");

	let absent = services
		.timeline
		.non_outlier_pdu_exists(incoming.event_id.as_ref())
		.await
		.is_not_found();

	assert!(absent, "current-state dependency failure reached the timeline");

	let retained = services
		.timeline
		.pdu_exists(incoming.event_id.as_ref())
		.await;

	assert!(retained, "current-state dependency failure lost the outlier");

	assert_no_memo(services, incoming.event_id.as_ref()).await
}

async fn verified_replaced_membership_ancestor(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
) -> Result<OwnedEventId> {
	let ancestor = member_event(services, room_id, user_id).await?;
	let content = RoomMemberEventContent::new(MembershipState::Join);
	let builder = PduBuilder::state(user_id.as_str(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;
	let membership = services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await?;

	drop(state_lock);
	let current = member_event(services, room_id, user_id).await?;

	if current.event_id != membership {
		return Err!("replacement membership did not become current room state");
	}

	let room_version = services.state.get_room_version(room_id).await?;

	services
		.auth_chain
		.event_ids_iter(room_id, &room_version, once(membership.as_ref()))
		.try_any(|event_id| ready(event_id == ancestor.event_id))
		.await?
		.then_ok_or_else(ancestor.event_id, || {
			err!("replaced membership is not an auth ancestor of its successor")
		})
}

async fn member_event(
	services: &Services,
	room_id: &RoomId,
	user_id: &UserId,
) -> Result<PduEvent> {
	services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.await
}

async fn prime_local_auth_chain(services: &Services, event_id: &EventId) -> Result<Vec<u8>> {
	services.clear_cache().await;

	let report = services
		.event_handler
		.local_state_report(event_id)
		.await?;

	assert_eq!(report.fallback, None, "auth-chain priming walk fell back");
	assert!(report.state_len.is_some(), "auth-chain priming walk produced no state");

	let cache = services.db.get("authchainkey_authchain")?;

	pin!(cache.raw_keys())
		.try_next()
		.await?
		.map(<[u8]>::to_vec)
		.ok_or_else(|| err!("auth-chain priming walk wrote no cache row"))
}

async fn held_message_fork(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<HeldFork> {
	let (left, left_json) = sign_message(services, user_id, room_id, "plain fork left").await?;
	let (right, right_json) =
		sign_message(services, user_id, room_id, "plain fork right").await?;

	let (top, top_json) = held_fork(
		services,
		user_id,
		room_id,
		(&left, &left_json),
		(&right, &right_json),
		"plain fork top",
	)
	.await?;

	Ok((left, right, top, top_json))
}
