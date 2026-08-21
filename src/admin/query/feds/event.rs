use std::{
	cmp::Ordering,
	collections::BTreeMap,
	fmt::{Result as FmtResult, Write as _},
	num::NonZeroUsize,
	time::{Duration, Instant},
};

use futures::{StreamExt, stream::iter as stream_iter};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, OwnedEventId, OwnedRoomOrAliasId, OwnedServerName,
	OwnedServerSigningKeyId, RoomVersionId,
	api::federation::event::get_event::v1::{Request, Response},
	canonical_json::{redact, redact_in_place},
	serde::{Base64, base64::Standard},
	signatures::{
		PublicKeyMap, PublicKeySet, Verified, content_hash,
		required_server_signatures_to_verify_event, verify_json as verify_signed_json,
	},
};
use tuwunel_core::{
	Err, Error, Result, err,
	matrix::{event::gen_event_id, room_version::rules as room_version_rules},
	utils::{stream::BroadbandExt, time::Elapsed},
};
use tuwunel_service::federation::feds::{Fault, Outcome};

use super::{SweepArgs, fault_message, markdown_cell, prepare, render_total_time};
use crate::{Context, admin_command};

pub(super) const WIDTH_DEFAULT: NonZeroUsize = NonZeroUsize::new(192).expect("192 is nonzero");

type SigningKeys = BTreeMap<OwnedServerName, Vec<OwnedServerSigningKeyId>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HashStatus {
	Valid,
	Redacted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Verification {
	hash: Option<HashStatus>,
	signature: bool,
}

#[admin_command]
pub(super) async fn feds_event(
	&self,
	event_id: OwnedEventId,
	room: Option<OwnedRoomOrAliasId>,
	verify_hash: bool,
	verify_signature: bool,
	sweep: SweepArgs,
) -> Result {
	let validation_width = sweep.width.unwrap_or(WIDTH_DEFAULT).get();

	let room = match room {
		| Some(room) => room,
		| None => self
			.services
			.timeline
			.get_pdu(&event_id)
			.await
			.map_err(|error| {
				err!(
					"Could not infer the room for {event_id}; supply the room explicitly: \
					 {error}"
				)
			})?
			.room_id
			.into(),
	};

	let prepared = prepare(self, &room, sweep, WIDTH_DEFAULT).await?;

	let room_version = self
		.services
		.state
		.get_room_version(&prepared.room_id)
		.await?;

	let started = Instant::now();
	let outcomes = self
		.services
		.federation
		.for_room(&prepared.room_id, |_| Request { event_id: event_id.clone() }, prepared.opts)
		.collect::<Vec<_>>()
		.await;

	let total = started.elapsed();

	let event_id = &event_id;
	let room_version = &room_version;
	let verified = stream_iter(outcomes)
		.broadn_then(validation_width, async |outcome| {
			let result = match outcome.result {
				| Ok(response) => validate_response(
					self,
					response,
					event_id,
					room_version,
					verify_hash,
					verify_signature,
				)
				.await
				.map_err(Fault::Error),
				| Err(fault) => Err(fault),
			};

			Outcome {
				origin: outcome.origin,
				elapsed: outcome.elapsed,
				result,
			}
		})
		.collect::<Vec<_>>()
		.await;

	let output = render(verified, total);

	self.write_str(&output).await
}

async fn validate_response(
	context: &Context<'_>,
	response: Response,
	event_id: &OwnedEventId,
	room_version: &RoomVersionId,
	verify_hash: bool,
	verify_signature: bool,
) -> Result<Verification> {
	let event: CanonicalJsonObject = serde_json::from_str(response.pdu.get())
		.map_err(|error| err!(BadServerResponse("Invalid event JSON: {error}")))?;

	let received_event_id = gen_event_id(&event, room_version)?;

	if received_event_id != *event_id {
		return Err!(BadServerResponse("Requested {event_id}, but received {received_event_id}"));
	}

	match (verify_hash, verify_signature) {
		| (true, true) => verify_hash_and_signatures(context, &event, room_version).await,
		| (true, false) => Ok(Verification {
			hash: Some(verify_content_hash(&event, room_version)?),
			signature: false,
		}),
		| (false, true) => {
			verify_signatures(context, &event, room_version).await?;

			Ok(Verification { hash: None, signature: true })
		},
		| (false, false) => Ok(Verification { hash: None, signature: false }),
	}
}

async fn verify_hash_and_signatures(
	context: &Context<'_>,
	event: &CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> Result<Verification> {
	let hash = match context
		.services
		.server_keys
		.verify_event(event, Some(room_version))
		.await?
	{
		| Verified::All => HashStatus::Valid,
		| Verified::Signatures => verify_content_hash(event, room_version)?,
	};

	Ok(Verification { hash: Some(hash), signature: true })
}

fn verify_content_hash(
	event: &CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> Result<HashStatus> {
	let expected = event
		.get("hashes")
		.and_then(CanonicalJsonValue::as_object)
		.and_then(|hashes| hashes.get("sha256"))
		.and_then(CanonicalJsonValue::as_str)
		.ok_or_else(|| err!(BadServerResponse("Event is missing hashes.sha256")))?;

	let expected = Base64::<Standard, [u8; 32]>::parse(expected)
		.map_err(|error| err!(BadServerResponse("Invalid hashes.sha256: {error}")))?;

	let calculated = content_hash(event).map_err(|error| {
		err!(BadServerResponse("Could not calculate the event content hash: {error}"))
	})?;

	if expected.as_bytes() == calculated.as_bytes() {
		return Ok(HashStatus::Valid);
	}

	if is_redacted(event, room_version)? {
		return Ok(HashStatus::Redacted);
	}

	Err!(BadServerResponse("Event content hash does not match the received content"))
}

fn is_redacted(event: &CanonicalJsonObject, room_version: &RoomVersionId) -> Result<bool> {
	let rules = room_version_rules(room_version)?;
	let mut redacted = event.clone();

	redacted.remove("unsigned");

	redact_in_place(&mut redacted, &rules.redaction, None).map_err(|error| {
		err!(BadServerResponse("Could not classify event redaction: {error}"))
	})?;

	Ok(event
		.iter()
		.filter(|(key, _)| key.as_str() != "unsigned")
		.eq(redacted.iter()))
}

#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		room_version = ?room_version,
	)
)]
async fn verify_signatures(
	context: &Context<'_>,
	event: &CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> Result {
	let (signed_event, keys) = signature_input(event, room_version)?;
	let (public_keys, key_error) = get_public_keys(context, &keys).await;

	verify_signature(&public_keys, &signed_event, key_error.as_ref())
}

async fn get_public_keys(
	context: &Context<'_>,
	keys: &SigningKeys,
) -> (PublicKeyMap, Option<Error>) {
	let mut public_keys = PublicKeyMap::new();
	let mut first_error = None;

	for (server, key_ids) in keys {
		let mut server_keys = PublicKeySet::new();

		for key_id in key_ids {
			match context
				.services
				.server_keys
				.get_verify_key(server, key_id)
				.await
			{
				| Ok(verify_key) => {
					server_keys.insert(key_id.as_str().into(), verify_key.key);
				},
				| Err(error) if first_error.is_none() => first_error = Some(error),
				| Err(_) => (),
			}
		}

		public_keys.insert(server.as_str().into(), server_keys);
	}

	(public_keys, first_error)
}

fn verify_signature(
	public_keys: &PublicKeyMap,
	event: &CanonicalJsonObject,
	key_error: Option<&Error>,
) -> Result {
	verify_signed_json(public_keys, event).map_err(|error| {
		key_error.map_or_else(
			|| err!(BadServerResponse("Signature verification failed: {error}")),
			|key_error| {
				err!(BadServerResponse(
					"Signature verification failed: {error}; signing-key acquisition also \
					 failed: {key_error}"
				))
			},
		)
	})
}

fn signature_input(
	event: &CanonicalJsonObject,
	room_version: &RoomVersionId,
) -> Result<(CanonicalJsonObject, SigningKeys)> {
	let rules = room_version_rules(room_version)?;
	let required = required_server_signatures_to_verify_event(event, &rules.signatures).map_err(
		|error| err!(BadServerResponse("Could not determine required signatures: {error}")),
	)?;

	let signatures = event
		.get("signatures")
		.and_then(CanonicalJsonValue::as_object)
		.ok_or_else(|| err!(BadServerResponse("Event is missing signatures")))?;

	let mut filtered = CanonicalJsonObject::new();
	let mut keys = SigningKeys::new();

	for server in required {
		let signature_set = signatures
			.get(server.as_str())
			.and_then(CanonicalJsonValue::as_object)
			.ok_or_else(|| {
				err!(BadServerResponse("Event is missing a signature object from {server}"))
			})?;

		let key_ids = signature_set
			.keys()
			.filter_map(|key_id| key_id.as_str().try_into().ok())
			.collect();

		filtered.insert(server.as_str().into(), signature_set.clone().into());
		keys.insert(server, key_ids);
	}

	let mut event = redact(event.clone(), &rules.redaction, None).map_err(|error| {
		err!(BadServerResponse("Could not redact the event for verification: {error}"))
	})?;

	event.insert("signatures".into(), filtered.into());

	Ok((event, keys))
}

fn render(mut outcomes: Vec<Outcome<Verification>>, total: Duration) -> String {
	outcomes.sort_unstable_by(outcome_order);

	let mut output = String::new();

	render_into(&mut output, &outcomes, total).expect("writing to a String cannot fail");
	output
}

fn outcome_order(left: &Outcome<Verification>, right: &Outcome<Verification>) -> Ordering {
	match (
		matches!(left.result, Err(Fault::NotAttempted)),
		matches!(right.result, Err(Fault::NotAttempted)),
	) {
		| (true, false) => Ordering::Greater,
		| (false, true) => Ordering::Less,
		| (true, true) => left.origin.cmp(&right.origin),
		| _ => left
			.elapsed
			.cmp(&right.elapsed)
			.then_with(|| left.origin.cmp(&right.origin)),
	}
}

fn render_into(
	output: &mut String,
	outcomes: &[Outcome<Verification>],
	total: Duration,
) -> FmtResult {
	writeln!(output, "| rank | origin | elapsed | hash | signature | fault |")?;
	writeln!(output, "| ---: | :--- | ---: | :--- | :--- | :--- |")?;

	let mut rank = 0_usize;

	for outcome in outcomes {
		match &outcome.result {
			| Err(fault @ Fault::NotAttempted) => {
				let fault = fault_message(fault);
				let fault = markdown_cell(&fault);

				writeln!(output, "| | {} | | | | {fault} |", outcome.origin)?;
			},
			| Err(fault) => {
				rank = rank.saturating_add(1);

				let fault = fault_message(fault);
				let fault = markdown_cell(&fault);

				writeln!(
					output,
					"| {rank} | {} | {} | | | {fault} |",
					outcome.origin,
					Elapsed::from(outcome.elapsed),
				)?;
			},
			| Ok(verification) => {
				rank = rank.saturating_add(1);

				let signature = if verification.signature { "ok" } else { "" };

				writeln!(
					output,
					"| {rank} | {} | {} | {} | {signature} | |",
					outcome.origin,
					Elapsed::from(outcome.elapsed),
					hash_cell(verification.hash),
				)?;
			},
		}
	}

	render_total_time(output, total)
}

fn hash_cell(status: Option<HashStatus>) -> &'static str {
	match status {
		| Some(HashStatus::Valid) => "ok",
		| Some(HashStatus::Redacted) => "redacted",
		| None => "",
	}
}

#[cfg(test)]
mod tests {
	use ruma::{ServerName, server_name};
	use serde_json::json;

	use super::*;

	#[test]
	fn content_hash_distinguishes_valid_redacted_and_invalid_events() {
		let mut event = event();
		let hash = content_hash(&event)
			.expect("event content should hash")
			.encode();

		event
			.get_mut("hashes")
			.and_then(CanonicalJsonValue::as_object_mut)
			.expect("test event should contain hashes")
			.insert("sha256".into(), hash.into());

		assert_eq!(
			verify_content_hash(&event, &RoomVersionId::V11).expect("valid event should verify"),
			HashStatus::Valid,
		);

		let rules =
			room_version_rules(&RoomVersionId::V11).expect("room version should be supported");

		let redacted =
			redact(event.clone(), &rules.redaction, None).expect("test event should redact");

		assert_eq!(
			verify_content_hash(&redacted, &RoomVersionId::V11)
				.expect("canonical redaction should be accepted"),
			HashStatus::Redacted,
		);

		let mut malformed = redacted;

		malformed
			.get_mut("hashes")
			.and_then(CanonicalJsonValue::as_object_mut)
			.expect("test event should contain hashes")
			.insert("sha256".into(), "not a hash".into());

		assert!(
			verify_content_hash(&malformed, &RoomVersionId::V11).is_err(),
			"redaction must not excuse a malformed hash",
		);

		event.insert("content".into(), json!({ "body": "modified" }).try_into().unwrap());
		assert!(
			verify_content_hash(&event, &RoomVersionId::V11).is_err(),
			"a mismatched unredacted event must fail",
		);
	}

	#[test]
	fn signature_input_selects_keys_and_bad_signature_is_an_error() {
		let mut event = event();

		event.remove("hashes");

		let (event, keys) = signature_input(&event, &RoomVersionId::V11)
			.expect("signature-only preparation should not require a hash");

		let signatures = event
			.get("signatures")
			.and_then(CanonicalJsonValue::as_object)
			.expect("prepared event should contain signatures");

		assert!(!event.contains_key("hashes"));
		assert!(signatures.contains_key("example.org"));
		assert!(!signatures.contains_key("elsewhere.example"));

		let key_ids = keys
			.get(server_name!("example.org"))
			.expect("origin keys should be requested");

		assert_eq!(key_ids.len(), 1);
		assert_eq!(key_ids[0].as_str(), "ed25519:1");
		assert!(!keys.contains_key(server_name!("elsewhere.example")));

		let error = verify_signature(&PublicKeyMap::new(), &event, None)
			.expect_err("missing public keys should fail signature verification");

		assert!(
			error
				.to_string()
				.contains("Signature verification failed")
		);
	}

	#[test]
	fn render_ranks_attempted_outcomes_by_latency_and_leaves_undispatched_unranked() {
		let outcomes = vec![
			success(server_name!("slow.example"), 40, HashStatus::Redacted),
			Outcome {
				origin: server_name!("alpha-skipped.example").to_owned(),
				elapsed: Duration::from_millis(100),
				result: Err(Fault::NotAttempted),
			},
			Outcome {
				origin: server_name!("skipped.example").to_owned(),
				elapsed: Duration::ZERO,
				result: Err(Fault::NotAttempted),
			},
			success(server_name!("fast.example"), 10, HashStatus::Valid),
			verification(server_name!("no-hash.example"), 30, None, true),
			verification(
				server_name!("no-signature.example"),
				35,
				Some(HashStatus::Valid),
				false,
			),
			verification(server_name!("unchecked.example"), 37, None, false),
			Outcome {
				origin: server_name!("timeout.example").to_owned(),
				elapsed: Duration::from_nanos(12_559_999),
				result: Err(Fault::Elapsed),
			},
		];

		let output = render(outcomes, Duration::from_millis(15_499));
		let fast = output
			.find("| 1 | fast.example | 10ms | ok | ok | |")
			.unwrap();

		let timeout = output
			.find("| 2 | timeout.example | 12.55ms | | | request deadline exceeded |")
			.unwrap();

		let no_hash = output
			.find("| 3 | no-hash.example | 30ms |  | ok | |")
			.unwrap();

		let no_signature = output
			.find("| 4 | no-signature.example | 35ms | ok |  | |")
			.unwrap();

		let unchecked = output
			.find("| 5 | unchecked.example | 37ms |  |  | |")
			.unwrap();

		let slow = output
			.find("| 6 | slow.example | 40ms | redacted | ok | |")
			.unwrap();

		let alpha_skipped = output
			.find("| | alpha-skipped.example | | | | sweep budget exhausted before dispatch |")
			.unwrap();

		let skipped = output
			.find("| | skipped.example | | | | sweep budget exhausted before dispatch |")
			.unwrap();

		assert!(
			fast < timeout
				&& timeout < no_hash
				&& no_hash < no_signature
				&& no_signature < unchecked
				&& unchecked < slow
				&& slow < alpha_skipped
				&& alpha_skipped < skipped
		);

		assert!(output.ends_with("\nFederation fanout took 15.49s.\n"));
	}

	fn event() -> CanonicalJsonObject {
		serde_json::from_value(json!({
			"auth_events": [],
			"content": { "body": "original" },
			"depth": 1,
			"hashes": { "sha256": "" },
			"origin_server_ts": 1,
			"prev_events": [],
			"sender": "@alice:example.org",
			"signatures": {
				"example.org": { "ed25519:1": "signature" },
				"elsewhere.example": { "ed25519:1": "signature" }
			},
			"type": "m.room.message"
		}))
		.expect("test event should be canonical JSON")
	}

	fn success(origin: &ServerName, elapsed_ms: u64, hash: HashStatus) -> Outcome<Verification> {
		verification(origin, elapsed_ms, Some(hash), true)
	}

	fn verification(
		origin: &ServerName,
		elapsed_ms: u64,
		hash: Option<HashStatus>,
		signature: bool,
	) -> Outcome<Verification> {
		Outcome {
			origin: origin.to_owned(),
			elapsed: Duration::from_millis(elapsed_ms),
			result: Ok(Verification { hash, signature }),
		}
	}
}
