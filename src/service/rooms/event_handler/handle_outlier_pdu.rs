use futures::TryStreamExt;
use ruma::{
	CanonicalJsonObject, EventId,
	events::{StateEventType, TimelineEventType},
	room_version_rules::RoomVersionRules,
	serde::Raw,
};
use serde::{Deserialize, Deserializer, de::Error as _};
use tuwunel_core::{
	Err, Result, debug, debug_info, err, implement, trace, utils::stream::IterStream,
};
use tuwunel_matrix::{
	Event, PduEvent, StateKey,
	room::state::resolution::{AuthCheckOutcome, FetchEvent, FetchState, auth_check},
	room_version,
};

use super::eval::Eval;

/// The auth events of one event, each read once from storage.
///
/// Both halves of the event's check read these copies: the state-independent
/// rules decode the stored form again, the state-dependent rules use the parsed
/// events.
#[derive(Clone, Copy)]
struct Dependencies<'a>(&'a [AuthEvent]);

/// One auth event in its stored form, parsed as it is read.
///
/// Parsing inside the read keeps storage's choice between an accepted and an
/// outlier copy, where a copy that does not parse loses to one that does.
struct AuthEvent {
	stored: Raw<PduEvent>,
	parsed: PduEvent,
}

#[implement(super::Service)]
#[cfg_attr(unabridged, tracing::instrument(
    name = "outlier",
    level = "debug",
    skip_all,
    fields(lev = %recursion_level)
))]
pub(super) async fn handle_outlier_pdu(
	&self,
	eval @ Eval {
		room_id,
		event_id,
		room_version,
		create_event_id,
		recursion_level,
		auth_events_known,
		..
	}: Eval<'_>,
	mut pdu_json: CanonicalJsonObject,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	debug!(?event_id, ?auth_events_known, %recursion_level, "handle outlier");

	// 1. Remove unsigned field
	pdu_json.remove("unsigned");

	// TODO: For RoomVersion6 we must check that Raw<..> is canonical do we
	// anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json
	// 2. Check signatures, otherwise drop
	// 3. check content hash, redact if doesn't match
	let pdu_json = match self
		.services
		.server_keys
		.verify_event(&pdu_json, Some(room_version))
		.await
	{
		| Ok(ruma::signatures::Verified::All) => pdu_json,
		| Ok(ruma::signatures::Verified::Signatures) => {
			// Redact
			debug_info!("Calculated hash does not match (redaction): {event_id}");
			let Some(rules) = room_version.rules() else {
				return Err!(Request(UnsupportedRoomVersion(
					"Cannot redact event for unknown room version {room_version:?}."
				)));
			};

			let Ok(pdu_json) = ruma::canonical_json::redact(pdu_json, &rules.redaction, None)
			else {
				return Err!(Request(InvalidParam("Redaction failed")));
			};

			// Skip the PDU if it is redacted and we already have it as an outlier event
			if self.services.timeline.pdu_exists(event_id).await {
				return Err!(Request(InvalidParam(
					"Event was redacted and we already knew about it"
				)));
			}

			pdu_json
		},
		| Err(e) => {
			return Err!(Request(InvalidParam(debug_error!(
				"Signature verification failed for {event_id}: {e}"
			))));
		},
	};

	// Now that we have checked the signature and hashes we can make mutations and
	// convert to our PduEvent type.
	let room_rules = room_version::rules(room_version)?;
	let (event, pdu_json) =
		PduEvent::from_object_federation(room_id, event_id, pdu_json, &room_rules)?;

	self.check_event_provenance(&event, &room_rules, Some(create_event_id))?;

	if !auth_events_known {
		// 4. fetch any missing auth events doing all checks listed here starting at 1.
		//    These are not timeline events
		// 5. Reject "due to auth events" if can't get all the auth events or some of
		//    the auth events are also rejected "due to auth events"
		// NOTE: Step 5 is not applied anymore because it failed too often
		debug!("Fetching auth events");
		let fetch = self.fetch_auth(eval, event.auth_events());

		cfg_select! {
			debug_assertions => Box::pin(fetch).await, // size firewall
			_ => fetch.await,
		}?;
	}

	// 6. Reject "due to auth events" if the event doesn't pass auth based on the
	//    auth events
	debug!("Checking based on auth events");
	self.auth_check_by_auth_events(&event, &room_rules)
		.await?
		.into_result()?;

	trace!("Validation successful.");

	// 7. Persist the event as an outlier.
	self.services
		.timeline
		.add_pdu_outlier(event.event_id(), &pdu_json);

	trace!("Added pdu as outlier.");

	Ok((event, pdu_json))
}

/// Checks an event against the stored auth events it names.
///
/// In room versions that derive the room ID from the create event, the create
/// event is also taken as an auth event; a create event itself reads none. Each
/// auth event is read once and both halves of the check use that copy, so
/// failing to read one fails the check rather than denying the event.
#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip_all, fields(event_id = %event.event_id()))]
pub(crate) async fn auth_check_by_auth_events(
	&self,
	event: &PduEvent,
	room_rules: &RoomVersionRules,
) -> Result<AuthCheckOutcome> {
	let is_hydra = !room_rules
		.event_format
		.allow_room_create_in_auth_events;

	let not_create = *event.kind() != TimelineEventType::RoomCreate;
	let hydra_create_id = (not_create && is_hydra)
		.then(|| event.room_id().as_event_id().ok())
		.flatten();

	// The create rule denies any named auth event without reading it.
	let auth_events: Vec<AuthEvent> = event
		.auth_events()
		.filter(|_| not_create)
		.chain(hydra_create_id.as_deref())
		.try_stream()
		.and_then(|auth_event_id| self.services.timeline.get(auth_event_id))
		.try_collect()
		.await?;

	let dependencies = Dependencies(&auth_events);

	auth_check(room_rules, event, dependencies, dependencies).await
}

impl FetchEvent for Dependencies<'_> {
	async fn get<T>(self, event_id: &EventId) -> Result<T>
	where
		T: for<'de> Deserialize<'de> + Send,
	{
		let AuthEvent { stored, .. } = self
			.find(event_id)
			.ok_or_else(|| err!(Database("Auth event {event_id} was not read.")))?;

		stored
			.deserialize_as_unchecked()
			.map_err(Into::into)
	}

	async fn exists(self, event_id: &EventId) -> Result<bool> {
		Ok(self.find(event_id).is_some())
	}
}

impl<'a> FetchState for Dependencies<'a> {
	type Pdu = &'a PduEvent;

	async fn get(self, event_type: StateEventType, state_key: StateKey) -> Result<Self::Pdu> {
		self.0
			.iter()
			.map(|AuthEvent { parsed, .. }| parsed)
			.find(|event| {
				event.state_key() == Some(state_key.as_str())
					&& event.event_type().to_cow_str() == event_type.to_cow_str()
			})
			.ok_or_else(|| {
				err!(Request(NotFound("Missing auth event {event_type:?},{state_key:?}")))
			})
	}
}

impl<'a> Dependencies<'a> {
	fn find(self, event_id: &EventId) -> Option<&'a AuthEvent> {
		self.0
			.iter()
			.find(|AuthEvent { parsed, .. }| parsed.event_id() == event_id)
	}
}

impl<'de> Deserialize<'de> for AuthEvent {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		let stored: Raw<PduEvent> = Deserialize::deserialize(deserializer)?;
		let parsed = stored.deserialize().map_err(D::Error::custom)?;

		Ok(Self { stored, parsed })
	}
}
