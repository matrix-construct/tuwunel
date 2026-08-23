use std::{future::ready, io::Error as IoError, sync::Arc};

use futures::future::join;
use ruma::{events::StateEventType, owned_event_id};
use tuwunel_core::{Result, matrix::StateKey};

use super::{
	DIVERGENCE_SAMPLE, DivergenceKey, DivergenceKeys, Fallback, ShadowOutcome, ShortStateKey,
	StateIds, StateLocalCounters, WalkAttempt, WalkOutcome, divergent, resolve_divergence_sample,
};

#[test]
fn settled_walks_partition_into_one_outcome() {
	let counters = Arc::new(StateLocalCounters::default());

	WalkAttempt::start(counters.clone()).settle(WalkOutcome::Resolved, 2);

	for fallback in [
		Fallback::Absent,
		Fallback::Ceiling,
		Fallback::AuthMissing,
		Fallback::AllCommitted,
		Fallback::Entries,
		Fallback::Canary,
		Fallback::CreateMismatch,
		Fallback::Unevaluable,
		Fallback::Error,
	] {
		WalkAttempt::start(counters.clone()).settle(WalkOutcome::Fallback(fallback), 1);
	}

	WalkAttempt::start(counters.clone()).settle(WalkOutcome::Failure, 3);
	drop(WalkAttempt::start(counters.clone()));

	let metrics = counters.snapshot();
	let fallbacks = [
		metrics.fallback_absent,
		metrics.fallback_ceiling,
		metrics.fallback_auth_missing,
		metrics.fallback_all_committed,
		metrics.fallback_entries,
		metrics.fallback_canary,
		metrics.fallback_create_mismatch,
		metrics.fallback_unevaluable,
		metrics.fallback_error,
	];

	let fallback_total: u64 = fallbacks.into_iter().sum();

	assert_eq!(metrics.walk_resolved, 1);
	assert_eq!(fallbacks, [1; 9]);
	assert_eq!(metrics.walk_failures, 2);
	assert_eq!(metrics.gate_denials, 14);
	assert_eq!(
		metrics.walk_attempts,
		metrics.walk_resolved + fallback_total + metrics.walk_failures
	);
}

#[test]
fn shadow_compares_partition_into_one_outcome() {
	let counters = StateLocalCounters::default();

	counters.settle_shadow(ShadowOutcome::Agreement);
	counters.settle_shadow(ShadowOutcome::Divergence);
	counters.settle_shadow(ShadowOutcome::Divergence);

	let metrics = counters.snapshot();

	assert_eq!(metrics.shadow_compares, 3);
	assert_eq!(metrics.shadow_agreements, 1);
	assert_eq!(metrics.shadow_divergences, 2);
	assert_eq!(metrics.shadow_compares, metrics.shadow_agreements + metrics.shadow_divergences);
}

#[tokio::test]
async fn divergence_reports_agreement_and_keeps_sides_separate() {
	let local = StateIds::from([(1, owned_event_id!("$shared")), (2, owned_event_id!("$local"))]);
	let equal = StateIds::from([(1, owned_event_id!("$shared")), (2, owned_event_id!("$local"))]);

	let fetched =
		StateIds::from([(1, owned_event_id!("$shared")), (3, owned_event_id!("$fetch"))]);

	let agreement = divergent(&local, &equal);
	let only_local = divergent(&local, &fetched);
	let only_fetch = divergent(&fetched, &local);
	let replaced_local = StateIds::from([(4, owned_event_id!("$local2"))]);
	let replaced_fetch = StateIds::from([(4, owned_event_id!("$fetch2"))]);
	let replaced_only_local = divergent(&replaced_local, &replaced_fetch);
	let replaced_only_fetch = divergent(&replaced_fetch, &replaced_local);

	assert_eq!(agreement.count, 0);
	assert!(agreement.sample.is_empty());
	assert_eq!(only_local.count, 1);
	assert_eq!(only_fetch.count, 1);
	assert_eq!(replaced_only_local.count, 1);
	assert_eq!(replaced_only_fetch.count, 1);
	assert_eq!(replaced_only_local.sample.as_slice(), &[4]);
	assert_eq!(replaced_only_fetch.sample.as_slice(), &[4]);

	let resolve = |shortstatekey: ShortStateKey| {
		let state_key = match shortstatekey {
			| 2 => "local",
			| 3 => "fetch",
			| _ => unreachable!(),
		};

		ready(Ok((StateEventType::RoomMember, state_key.into())))
	};

	let (only_local, only_fetch) = join(
		resolve_divergence_sample(only_local.sample, &resolve),
		resolve_divergence_sample(only_fetch.sample, &resolve),
	)
	.await;

	let expected_local = DivergenceKey::Resolved(StateEventType::RoomMember, "local".into());
	let expected_fetch = DivergenceKey::Resolved(StateEventType::RoomMember, "fetch".into());

	assert_eq!(only_local.as_slice(), &[expected_local]);
	assert_eq!(only_fetch.as_slice(), &[expected_fetch]);
}

#[test]
fn divergence_samples_are_capped_per_side() {
	let count = 11_u64;
	let local: StateIds = (0..count)
		.map(|shortstatekey| (shortstatekey, owned_event_id!("$local")))
		.collect();

	let fetched: StateIds = (count..count * 2)
		.map(|shortstatekey| (shortstatekey, owned_event_id!("$fetch")))
		.collect();

	let only_local = divergent(&local, &fetched);
	let only_fetch = divergent(&fetched, &local);

	assert_eq!(DIVERGENCE_SAMPLE, 8);
	assert_eq!(only_local.count, 11);
	assert_eq!(only_fetch.count, 11);
	assert_eq!(only_local.sample.len(), 8);
	assert_eq!(only_fetch.sample.len(), 8);
	assert!(
		only_local
			.sample
			.iter()
			.all(|short| *short < count)
	);
	assert!(
		only_fetch
			.sample
			.iter()
			.all(|short| *short >= count)
	);
}

#[tokio::test]
async fn unresolved_sample_keeps_its_raw_shortstatekey() {
	let sample: DivergenceKeys = [6, 7, 8].into_iter().collect();
	let resolve = |shortstatekey: ShortStateKey| {
		let result: Result<(StateEventType, StateKey)> = if shortstatekey == 7 {
			Err(IoError::other("injected short state key failure").into())
		} else {
			let state_key = StateKey::from(shortstatekey.to_string());

			Ok((StateEventType::RoomMember, state_key))
		};

		ready(result)
	};

	let resolved = resolve_divergence_sample(sample, &resolve).await;
	let expected = [
		DivergenceKey::Resolved(StateEventType::RoomMember, "6".into()),
		DivergenceKey::UnresolvedShortStateKey(7),
		DivergenceKey::Resolved(StateEventType::RoomMember, "8".into()),
	];

	assert_eq!(resolved.as_slice(), &expected);
}
