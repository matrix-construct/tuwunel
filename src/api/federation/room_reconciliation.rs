//! MSC4521 algebraic set-reconciliation endpoints.

use std::collections::BTreeSet;

use axum::extract::State;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rezzy_recon::{
	AlgebraicError, BucketRequest, MAX_BUCKETS_PER_ROUND, RoomAccumulator, SyndromeSketch,
	build_bucket_sketches, encode_strata, triage::validate_bucket_requests,
};
use ruma::{OwnedEventId, RoomId, ServerName};
use tuwunel_core::{Result, err, utils::hash::sha256};
use tuwunel_service::rooms::timeline::reconciliation::RoomReconciliationState;

use super::{
	AccessCheck,
	reconciliation_api::{get_room_digest, get_room_digest_strata, post_room_diff},
};
use crate::Ruma;

const DIGEST_TYPE: &str = "algebraic_v1.0.0";

fn frame_id_for(event_ids: &[OwnedEventId]) -> String {
	let ids: BTreeSet<&OwnedEventId> = event_ids.iter().collect();
	let json = serde_json::to_vec(&ids).expect("event IDs always serialize as JSON strings");
	URL_SAFE_NO_PAD.encode(sha256::hash(json))
}

async fn room_snapshot(
	services: &crate::State,
	origin: &ServerName,
	room_id: &RoomId,
) -> Result<RoomReconciliationState> {
	AccessCheck {
		services,
		origin,
		room_id,
		event_id: None,
	}
	.check()
	.await?;
	services
		.rooms
		.timeline
		.reconciliation_state(room_id)
		.await
}

fn common_digest_fields(state: &RoomReconciliationState) -> (String, u64, String) {
	(
		state.resident.accumulator().encode_digest(),
		state.resident.accumulator().known_event_count(),
		frame_id_for(&state.frame_event_ids),
	)
}

pub(crate) async fn get_room_reconciliation_digest_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_digest::Request>,
) -> Result<get_room_digest::Response> {
	let state = room_snapshot(&services, body.origin(), &body.room_id).await?;
	let (digest, known_event_count, frame_id) = common_digest_fields(&state);
	Ok(get_room_digest::Response {
		digest,
		digest_type: DIGEST_TYPE.to_owned(),
		known_event_count,
		frame_id,
		extremity_event_ids: state.frame_event_ids.clone(),
		frame_event_ids: state.frame_event_ids,
		depth_range: state.depth_range.to_vec(),
		origin_server_ts_range: state.origin_server_ts_range.to_vec(),
	})
}

pub(crate) async fn get_room_reconciliation_strata_route(
	State(services): State<crate::State>,
	body: Ruma<get_room_digest_strata::Request>,
) -> Result<get_room_digest_strata::Response> {
	let state = room_snapshot(&services, body.origin(), &body.room_id).await?;
	let (digest, known_event_count, frame_id) = common_digest_fields(&state);
	Ok(get_room_digest_strata::Response {
		digest,
		digest_type: DIGEST_TYPE.to_owned(),
		known_event_count,
		frame_id,
		extremity_event_ids: state.frame_event_ids.clone(),
		frame_event_ids: state.frame_event_ids,
		depth_range: state.depth_range.to_vec(),
		origin_server_ts_range: state.origin_server_ts_range.to_vec(),
		strata: encode_strata(state.resident.strata()),
	})
}

fn bad_param(what: &str) -> tuwunel_core::Error {
	err!(Request(InvalidParam("Invalid or missing `{what}`.")))
}

struct SketchRequest {
	bucket: BucketRequest,
	local_sketch: SyndromeSketch,
}

struct ValidatedDiff {
	frame_negotiation: bool,
	frame_event_ids: Vec<OwnedEventId>,
	frame_id: String,
	local_digest: u128,
	local_known_event_count: u64,
	scope: Option<String>,
	state_at: Option<OwnedEventId>,
	requests: Vec<SketchRequest>,
}

fn validate_diff(body: &post_room_diff::Request) -> Result<ValidatedDiff> {
	if body.mode != "sketch" {
		return Err!(Request(InvalidParam("Only `sketch` mode is supported.")));
	}
	if body.digest_type != DIGEST_TYPE {
		return Err!(Request(InvalidParam("Unsupported `digest_type`.")));
	}
	match body.scope.as_deref().unwrap_or("event_set") {
		| "event_set" => {},
		| "resolved_state" => {
			return Err!(Request(InvalidParam("`resolved_state` scope is not implemented.")));
		},
		| _ => return Err!(Request(InvalidParam("Unknown `scope`."))),
	}
	if body.frame_negotiation && body.frame_event_ids.is_empty() {
		return Err!(Request(InvalidParam(
			"`frame_event_ids` is required when `frame_negotiation` is true."
		)));
	}
	let local_digest = RoomAccumulator::decode_digest(&body.local_digest)
		.map_err(|_| bad_param("local_digest"))?;
	if body.requests.len() != body.local_sketches.len() {
		return Err!(Request(InvalidParam(
			"`local_sketches` length must match `requests` length."
		)));
	}
	if body.requests.len() > MAX_BUCKETS_PER_ROUND {
		return Err!(Request(InvalidParam("Too many bucket requests.")));
	}
	let buckets = body
		.requests
		.iter()
		.map(|bucket| {
			let depth = u8::try_from(bucket.depth).map_err(|_| bad_param("requests"))?;
			let prefix = bucket
				.prefix
				.parse()
				.map_err(|_| bad_param("requests"))?;
			let capacity = usize::try_from(bucket.capacity).map_err(|_| bad_param("requests"))?;
			Ok(BucketRequest::new(depth, prefix, capacity))
		})
		.collect::<Result<Vec<_>>>()?;
	validate_bucket_requests(&buckets).map_err(|_| bad_param("requests"))?;
	let requests = buckets
		.iter()
		.zip(&body.local_sketches)
		.map(|(bucket, sketch)| {
			let local_sketch = SyndromeSketch::decode(bucket.capacity, sketch)
				.map_err(|_| bad_param("local_sketches"))?;
			Ok(SketchRequest { bucket: *bucket, local_sketch })
		})
		.collect::<Result<Vec<_>>>()?;
	Ok(ValidatedDiff {
		frame_negotiation: body.frame_negotiation,
		frame_event_ids: body.frame_event_ids.clone(),
		frame_id: body.frame_id.clone(),
		local_digest,
		local_known_event_count: body.local_known_event_count,
		scope: body.scope.clone(),
		state_at: body.state_at.clone(),
		requests,
	})
}

struct DiffOutcome {
	missing_event_ids: Vec<OwnedEventId>,
	requester_only_short_ids: Vec<String>,
	expected_requester_side_accumulator: String,
}

enum Sketched {
	Decoded(DiffOutcome),
	CapacityExceeded,
}

fn encode_short_id(id: u64) -> String { URL_SAFE_NO_PAD.encode(id.to_be_bytes()) }

fn diff_buckets(state: &RoomReconciliationState, body: &ValidatedDiff) -> Result<Sketched> {
	let buckets: Vec<BucketRequest> = body.requests.iter().map(|r| r.bucket).collect();
	let residual_digest = state.resident.accumulator().digest() ^ body.local_digest;
	if residual_digest == 0
		&& body.local_known_event_count == state.resident.accumulator().known_event_count()
	{
		return Ok(Sketched::Decoded(DiffOutcome {
			missing_event_ids: Vec::new(),
			requester_only_short_ids: Vec::new(),
			expected_requester_side_accumulator: RoomAccumulator::from_parts(0, 0)
				.encode_digest(),
		}));
	}
	let sketches = build_bucket_sketches(state.population.h64s(), &buckets)
		.map_err(|_| bad_param("requests"))?;
	let mut responder_side = RoomAccumulator::new();
	let mut outcome = DiffOutcome {
		missing_event_ids: Vec::new(),
		requester_only_short_ids: Vec::new(),
		expected_requester_side_accumulator: String::new(),
	};
	for (request, responder_sketch) in body.requests.iter().zip(&sketches) {
		let residual = responder_sketch
			.subtract(&request.local_sketch)
			.map_err(|_| err!(Request(InvalidParam("Invalid sketch length."))))?;
		if residual
			.coordinates()
			.iter()
			.all(|&coordinate| coordinate == 0)
		{
			continue;
		}
		let roots = match residual.decode_elements(request.bucket.capacity) {
			| Ok(roots) => roots,
			| Err(AlgebraicError::DecodeFailure | AlgebraicError::BudgetExhausted) => {
				return Ok(Sketched::CapacityExceeded);
			},
			| Err(_) => return Err!(Request(InvalidParam("Sketch decode failed."))),
		};
		for root in roots {
			if let Some(events) = state.h64_to_events.get(&root) {
				for (event_id, hash) in events {
					outcome.missing_event_ids.push(event_id.clone());
					responder_side
						.insert(*hash)
						.map_err(|e| err!(Database("repair set accumulator: {e:?}")))?;
				}
			} else {
				outcome
					.requester_only_short_ids
					.push(encode_short_id(root));
			}
		}
	}
	outcome.expected_requester_side_accumulator =
		RoomAccumulator::from_parts(residual_digest ^ responder_side.digest(), 0).encode_digest();
	Ok(Sketched::Decoded(outcome))
}

fn response(
	state: &RoomReconciliationState,
	body: ValidatedDiff,
	frame_status: &str,
	negotiated_frame_event_ids: Option<Vec<OwnedEventId>>,
	sketch_status: &str,
	outcome: Option<DiffOutcome>,
) -> post_room_diff::Response {
	let truncated = outcome.is_none() && sketch_status != "not_applicable";
	let (missing_event_ids, requester_only_short_ids, expected) = match outcome {
		| Some(outcome) => (
			outcome.missing_event_ids,
			Some(outcome.requester_only_short_ids),
			Some(outcome.expected_requester_side_accumulator),
		),
		| None => (Vec::new(), None, None),
	};
	post_room_diff::Response {
		digest_type: DIGEST_TYPE.to_owned(),
		missing_event_ids,
		requester_only_short_ids,
		expected_requester_side_accumulator: expected,
		remote_known_event_count: state.resident.accumulator().known_event_count(),
		remote_extremity_event_ids: state.frame_event_ids.clone(),
		frame_id: frame_id_for(&state.frame_event_ids),
		frame_status: frame_status.to_owned(),
		negotiated_frame_event_ids,
		scope: body.scope,
		state_at: body.state_at,
		sketch_status: sketch_status.to_owned(),
		truncated,
	}
}

pub(crate) async fn post_room_reconciliation_diff_route(
	State(services): State<crate::State>,
	body: Ruma<post_room_diff::Request>,
) -> Result<post_room_diff::Response> {
	let validated = validate_diff(&body)?;
	let room_id = body.room_id.clone();
	let state = room_snapshot(&services, body.origin(), &room_id).await?;
	let current_frame = frame_id_for(&state.frame_event_ids);
	if validated.frame_id != current_frame {
		return Err!(Request(InvalidParam(
			"Requested `frame_id` does not match the current frame."
		)));
	}
	let requested: BTreeSet<&OwnedEventId> = validated.frame_event_ids.iter().collect();
	let common: Vec<OwnedEventId> = state
		.frame_event_ids
		.iter()
		.filter(|id| requested.is_empty() || requested.contains(id))
		.cloned()
		.collect();
	let frame_status = match (validated.frame_negotiation, common.is_empty()) {
		| (false, _) => "not_requested",
		| (true, false) => "common",
		| (true, true) => "none",
	};
	let negotiated = validated
		.frame_negotiation
		.then_some(common.clone());
	if common.is_empty() {
		return Ok(response(&state, validated, frame_status, negotiated, "not_applicable", None));
	}
	Ok(match diff_buckets(&state, &validated)? {
		| Sketched::Decoded(outcome) =>
			response(&state, validated, frame_status, negotiated, "decoded", Some(outcome)),
		| Sketched::CapacityExceeded =>
			response(&state, validated, frame_status, negotiated, "capacity_exceeded", None),
	})
}
