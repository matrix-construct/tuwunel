use axum::extract::State;
use futures::StreamExt;
use ruma::{MilliSecondsSinceUnixEpoch, api::client::push::get_notifications, push::Action};
use tuwunel_core::{
	Result, at, err,
	matrix::{Event, PduId},
	utils::{
		stream::{ReadyExt, WidebandExt},
		string::to_small_string,
	},
};

use crate::Ruma;

/// # `GET /_matrix/client/r0/notifications/`
///
/// Paginate through the list of events the user has been, or would have been
/// notified about.
pub(crate) async fn get_notifications_route(
	State(services): State<crate::State>,
	body: Ruma<get_notifications::v3::Request>,
) -> Result<get_notifications::v3::Response> {
	use get_notifications::v3::Notification;

	let sender_user = body.sender_user();

	let from = body
		.body
		.from
		.as_deref()
		.map(str::parse)
		.transpose()
		.map_err(|e| err!(Request(InvalidParam("Invalid `from' parameter: {e}"))))?;

	let limit: usize = body
		.body
		.limit
		.map(TryInto::try_into)
		.transpose()?
		.unwrap_or(50)
		.clamp(1, 100);

	let only_highlight = body
		.body
		.only
		.as_deref()
		.is_some_and(|only| only.contains("highlight"));

	let mut next_token: Option<u64> = None;
	let notifications = services
		.pusher
		.get_notifications(sender_user, from)
		.ready_filter(|(_, notify)| {
			if only_highlight && !notify.actions.iter().any(Action::is_highlight) {
				return false;
			}

			true
		})
		.wide_filter_map(async |(count, notify)| {
			let pdu_id = PduId {
				shortroomid: notify.sroomid,
				count: count.into(),
			};

			let mut event = services
				.timeline
				.get_pdu_from_id(&pdu_id.into())
				.await
				.ok()
				.filter(|event| !event.is_redacted())?;
			if event.sender() != sender_user {
				event.remove_transaction_id().ok();
			}

			let read = services
				.pusher
				.last_notification_read(sender_user, event.room_id())
				.await
				.is_ok_and(|last_read| last_read.ge(&count));

			let ts = notify
				.ts
				.try_into()
				.map(MilliSecondsSinceUnixEpoch)
				.ok()?;

			let notification = Notification {
				room_id: event.room_id().into(),
				event: event.into_format(),
				ts,
				read,
				profile_tag: notify.tag,
				actions: notify.actions,
			};

			Some((count, notification))
		})
		.take(limit)
		.inspect(|(count, _)| {
			next_token.replace(*count);
		})
		.map(at!(1))
		.collect::<Vec<_>>()
		.await;

	Ok(get_notifications::v3::Response {
		next_token: next_token.map(to_small_string),
		notifications,
	})
}
