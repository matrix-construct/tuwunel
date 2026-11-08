use axum::extract::{MatchedPath, State};
use ruma::{
	UserId,
	api::client::delayed_events::update_delayed_event::{
		UpdateAction,
		unstable_v1::{Request as BodyRequest, Response as BodyResponse},
		v1::{Request, Response},
	},
};
use tuwunel_core::{Err, Result};
use tuwunel_service::Services;

use super::is_stable;
use crate::Ruma;

/// # `POST /_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delayId}/{action}`
///
/// Restarts, cancels or sends a scheduled event (MSC4140).
///
/// On the unstable path the access token may be omitted, since the `delay_id`
/// alone is hard to guess and delegated callers such as lk-jwt-service hold no
/// token. The stable path requires one.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn update_delayed_event_route(
	State(services): State<crate::State>,
	path: MatchedPath,
	body: Ruma<Request>,
) -> Result<Response> {
	let owner = body.sender_user.as_deref();

	if owner.is_none() && is_stable(&path) {
		return Err!(Request(MissingToken("Missing access token.")));
	}

	update(&services, &body.delay_id, &body.action, owner)
		.await
		.map(|()| Response::default())
}

/// # `POST /_matrix/client/unstable/org.matrix.msc4140/delayed_events/{delayId}`
///
/// Restarts, cancels or sends a scheduled event named in the body (MSC4140).
///
/// This is an earlier unstable form of the request, with the `action` in the
/// body rather than the path.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn update_delayed_event_by_body_route(
	State(services): State<crate::State>,
	body: Ruma<BodyRequest>,
) -> Result<BodyResponse> {
	update(&services, &body.delay_id, &body.action, Some(body.sender_user()))
		.await
		.map(|()| BodyResponse::new())
}

#[tracing::instrument(level = "debug", skip(services))]
async fn update(
	services: &Services,
	delay_id: &str,
	action: &UpdateAction,
	owner: Option<&UserId>,
) -> Result {
	let delayed_events = &services.delayed_events;

	match action {
		| UpdateAction::Restart => delayed_events.restart(delay_id, owner).await,
		| UpdateAction::Cancel => delayed_events.cancel(delay_id, owner).await,
		| UpdateAction::Send => delayed_events
			.send(delay_id, owner)
			.await
			.map(|_| ()),
		| _ => Err!(Request(NotFound("Unknown delayed event action."))),
	}
}
