//! Federation requests sent to the server under test, signed by itself or by
//! a peer the test plays.
//!
//! It stands apart from the client harness, so only the test binaries that
//! declare `mod federation` compile it. The server serves each request as it
//! would a peer's.

use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{
	Result,
	ruma::{
		EventId, OwnedEventId, OwnedRoomId, RoomId, ServerName, UserId,
		api::{
			OutgoingRequest, OutgoingRequestExt,
			federation::authentication::{ServerSignatures, ServerSignaturesInput},
			path_builder::SinglePath,
		},
		events::room::member::{MembershipState, RoomMemberEventContent},
		signatures::Ed25519KeyPair,
	},
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

/// A membership event signed by the server, in the form a peer submits it.
///
/// The join, leave and knock routes take the ID in their path and the PDU as
/// their body.
pub(crate) struct MembershipPdu {
	/// The event's ID, which the submitted form leaves out.
	pub(crate) event_id: OwnedEventId,

	/// The event as a membership route receives it.
	pub(crate) pdu: Box<RawJsonValue>,
}

/// Submit a membership event to the route whose request `new` builds.
///
/// The join, leave and knock routes each take a room, an event ID and the PDU;
/// the status and body come back as from `send`.
pub(crate) async fn submit<New, T>(
	services: &Services,
	base: &str,
	new: New,
	room_id: &RoomId,
	event_id: &EventId,
	pdu: Box<RawJsonValue>,
) -> Result<(u16, String)>
where
	New: FnOnce(OwnedRoomId, OwnedEventId, Box<RawJsonValue>) -> T,
	T: OutgoingRequest<Authentication = ServerSignatures, PathBuilder = SinglePath>,
{
	let request = new(room_id.to_owned(), event_id.to_owned(), pdu);

	send(services, base, request).await
}

/// Send a federation request signed by the server under test, to itself.
///
/// The status and raw body come back whatever the status, so a caller asserts
/// on a refusal as readily as on a success.
pub(crate) async fn send<T>(services: &Services, base: &str, request: T) -> Result<(u16, String)>
where
	T: OutgoingRequest<Authentication = ServerSignatures, PathBuilder = SinglePath>,
{
	let origin = services.globals.server_name();
	let key_pair = services.server_keys.keypair();

	send_as(services, base, origin, key_pair, request).await
}

/// Send a federation request to the server under test from another origin.
///
/// The request is signed with the origin's key pair, whose public key the
/// server must find at the origin; the status and raw body come back as from
/// `send`.
#[tracing::instrument(level = "debug", skip_all, fields(%origin))]
pub(crate) async fn send_as<T>(
	services: &Services,
	base: &str,
	origin: &ServerName,
	key_pair: &Ed25519KeyPair,
	request: T,
) -> Result<(u16, String)>
where
	T: OutgoingRequest<Authentication = ServerSignatures, PathBuilder = SinglePath>,
{
	let destination = services.globals.server_name().to_owned();
	let auth = ServerSignaturesInput::new(origin.to_owned(), destination, key_pair);
	let request = request.try_into_http_request::<Vec<u8>>(base, auth, ())?;
	let response = services
		.client
		.clients
		.default
		.execute(request.try_into()?)
		.await?;

	let status = response.status().as_u16();
	let body = response.text().await?;

	Ok((status, body))
}

/// Build and sign a local user's membership event without appending it.
///
/// The room's current state authorises the event, and the result is what a
/// peer would submit for its user through a membership route.
pub(crate) async fn prepare(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	membership: MembershipState,
) -> Result<MembershipPdu> {
	let room_version = services.state.get_room_version(room_id).await?;
	let content = RoomMemberEventContent::new(membership);
	let builder = PduBuilder::state(user_id.to_string(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;
	let (event, json) = services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await?;

	drop(state_lock);

	let pdu = services
		.federation
		.format_pdu_into(json, Some(&room_version))
		.await;

	Ok(MembershipPdu { event_id: event.event_id, pdu })
}
