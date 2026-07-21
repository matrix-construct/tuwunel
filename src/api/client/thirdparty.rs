use axum::extract::State;
use ruma::api::client::thirdparty::{
	get_location_for_protocol, get_location_for_room_alias, get_protocol, get_protocols,
	get_user_for_protocol, get_user_for_user_id,
};
use tuwunel_core::{Result, err};

use crate::Ruma;

/// # `GET /_matrix/client/v3/thirdparty/protocols`
///
/// Fetches the third-party network protocols advertised by registered
/// appservices. Bridges are queried only for protocols named in their
/// registration, and overlapping declarations are combined under one protocol
/// id.
pub(crate) async fn get_protocols_route(
	State(services): State<crate::State>,
	_body: Ruma<get_protocols::v3::Request>,
) -> Result<get_protocols::v3::Response> {
	let protocols = services
		.appservice
		.thirdparty_protocols(None)
		.await;

	Ok(get_protocols::v3::Response { protocols })
}

/// # `GET /_matrix/client/v3/thirdparty/protocol/{protocol}`
///
/// Fetches metadata from every appservice declaring the requested protocol.
/// The route returns `M_NOT_FOUND` when no registered bridge advertises it or
/// none of the declaring bridges returns a successfully decoded response.
pub(crate) async fn get_protocol_route(
	State(services): State<crate::State>,
	body: Ruma<get_protocol::v3::Request>,
) -> Result<get_protocol::v3::Response> {
	let protocol = services
		.appservice
		.thirdparty_protocols(Some(body.protocol.as_str()))
		.await
		.remove(&body.protocol)
		.ok_or_else(|| err!(Request(NotFound("Unknown protocol."))))?;

	Ok(get_protocol::v3::Response { protocol })
}

/// # `GET /_matrix/client/v3/thirdparty/user/{protocol}`
///
/// Looks up third-party users through every appservice declaring `protocol`.
/// Query fields are forwarded without the client's access token, and one
/// bridge's failure does not discard results supplied by another.
pub(crate) async fn get_user_for_protocol_route(
	State(services): State<crate::State>,
	body: Ruma<get_user_for_protocol::v3::Request>,
) -> Result<get_user_for_protocol::v3::Response> {
	let users = services
		.appservice
		.thirdparty_users(&body.protocol, &body.fields)
		.await;

	Ok(get_user_for_protocol::v3::Response { users })
}

/// # `GET /_matrix/client/v3/thirdparty/location/{protocol}`
///
/// Looks up third-party locations on a protocol via the registered appservices.
/// Successful bridge responses retain appservice-id order and the order of the
/// locations within each response.
pub(crate) async fn get_location_for_protocol_route(
	State(services): State<crate::State>,
	body: Ruma<get_location_for_protocol::v3::Request>,
) -> Result<get_location_for_protocol::v3::Response> {
	let locations = services
		.appservice
		.thirdparty_locations(&body.protocol, &body.fields)
		.await;

	Ok(get_location_for_protocol::v3::Response { locations })
}

/// # `GET /_matrix/client/v3/thirdparty/user`
///
/// Reverse third-party lookup by Matrix user id. Tuwunel does no server-side
/// namespace routing for these, so the result is empty, matching Synapse.
pub(crate) async fn get_user_for_user_id_route(
	_body: Ruma<get_user_for_user_id::v3::Request>,
) -> Result<get_user_for_user_id::v3::Response> {
	Ok(get_user_for_user_id::v3::Response { users: Vec::new() })
}

/// # `GET /_matrix/client/v3/thirdparty/location`
///
/// Reverse third-party lookup by Matrix room alias; empty for the same reason
/// as the user reverse lookup.
pub(crate) async fn get_location_for_room_alias_route(
	_body: Ruma<get_location_for_room_alias::v3::Request>,
) -> Result<get_location_for_room_alias::v3::Response> {
	Ok(get_location_for_room_alias::v3::Response { locations: Vec::new() })
}
