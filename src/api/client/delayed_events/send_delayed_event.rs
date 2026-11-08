use axum::extract::{MatchedPath, State};
use futures::FutureExt;
use ruma::{
	api::client::delayed_events::send_delayed_event::v3::{Request, Response},
	events::{MessageLikeEventType, StateEventType},
};
use tuwunel_core::Result;
use tuwunel_service::delayed_events::DelayId;

use super::{Kind, Schedule, is_stable, schedule};
use crate::Ruma;

/// # `PUT /_matrix/client/unstable/org.matrix.msc4140/rooms/{roomId}/delayed_event/{eventType}/{txnId}`
///
/// Schedules an event to be sent after `delay_ms` (MSC4140).
///
/// A `state_key` in the body makes it a state event. The stable path answers
/// with the stable error codes.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn send_delayed_event_route(
	State(services): State<crate::State>,
	path: MatchedPath,
	body: Ruma<Request>,
) -> Result<Response> {
	let event_type = body.event_type.to_cow_str();
	let message_type = MessageLikeEventType::from(&*event_type);
	let state_type = StateEventType::from(&*event_type);
	let kind = match body.state_key.as_deref() {
		| None => Kind::Message {
			event_type: &message_type,
			content: body.content.cast_ref_unchecked(),
		},
		| Some(state_key) => Kind::State {
			event_type: &state_type,
			state_key,
			content: body.content.cast_ref_unchecked(),
		},
	};

	let request = Schedule {
		sender: body.sender_user(),
		device: body.sender_device.as_deref(),
		room_id: &body.room_id,
		kind,
		delay: body.delay_ms,
		txn_id: Some(&body.txn_id),
		timestamp: body.appservice_info.as_ref().and(body.ts),
		stable: is_stable(&path),
	};

	schedule(&services, request)
		.boxed() // size firewall
		.await
		.map(DelayId::into_string)
		.map(Response::new)
}
