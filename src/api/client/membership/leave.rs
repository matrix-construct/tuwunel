use axum::extract::State;
use futures::FutureExt;
use ruma::api::client::membership::leave_room::v3::{Request, Response};
use tuwunel_core::{Err, Result};

use crate::{Ruma, client::admin::misc::is_notice_room};

/// Leaves a room through `POST /_matrix/client/v3/rooms/{roomId}/leave`.
///
/// Server-notice invitations cannot be rejected, but joined recipients can leave.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn leave_room_route(
	State(services): State<crate::State>,
	Ruma { body, sender_user, .. }: Ruma<Request>,
) -> Result<Response> {
	let state_lock = services.state.mutex.lock(&body.room_id).await;
	let sender = sender_user
		.as_deref()
		.expect("user must be authenticated for this handler");

	if services
		.state_cache
		.is_invited(sender, &body.room_id)
		.await && is_notice_room(&services, sender, &body.room_id).await?
	{
		return Err!(Request(CannotLeaveServerNoticeRoom("You cannot reject this invite")));
	}

	services
		.membership
		.leave(sender, &body.room_id, body.reason, false, &state_lock)
		.await?;

	if services.config.delete_rooms_after_leave {
		services
			.delete
			.delete_if_empty_local(&body.room_id, state_lock)
			.boxed()
			.await;
	}

	Ok(Response {})
}
