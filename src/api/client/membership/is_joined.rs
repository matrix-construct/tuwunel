use axum::extract::State;
use futures::future::Either::{Left, Right};
use ruma::api::client::membership::is_joined::unstable::{Request, Response};
use tuwunel_core::{Err, Result};
use tuwunel_service::appservice::IS_JOINED_SCOPE;

use crate::Ruma;

/// # `GET /_matrix/client/unstable/io.element.msc4502/rooms/{roomId}/is_joined`
///
/// Answers whether one user or one server is joined to a room (MSC4502).
///
/// Only an appservice that registered the scope may ask, masquerading or not.
/// A room this server is not in answers `false` for every subject, since its
/// membership there is no longer kept current. A failed read is an error, so
/// a storage fault never denies a real member.
pub(crate) async fn is_joined_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	let granted = body
		.appservice_info
		.as_ref()
		.is_some_and(|appservice| appservice.has_scope(IS_JOINED_SCOPE));

	if !granted {
		return Err!(Request(Forbidden("Missing {IS_JOINED_SCOPE} scope.")));
	}

	let room_id = &body.room_id;
	let state_cache = &services.state_cache;
	let subject_joined = match (body.mxid.as_deref(), body.server_name.as_deref()) {
		| (Some(user_id), None) => Left(state_cache.try_is_joined(user_id, room_id)),
		| (None, Some(server)) => Right(state_cache.server_in_room_result(server, room_id)),
		| _ => return Err!(Request(MissingParam("Pass exactly one of mxid and server_name."))),
	};

	let joined = subject_joined.await?
		&& state_cache
			.server_in_room_result(services.globals.server_name(), room_id)
			.await?;

	Ok(Response::new(joined))
}
