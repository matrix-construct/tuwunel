use axum::extract::State;
use futures::StreamExt;
use ruma::{
	DeviceId, UserId,
	api::client::dehydrated_device::get_events::{
		unstable_v1::{Request as ByBodyRequest, Response as ByBodyResponse},
		unstable_v2::{Request, Response},
	},
	events::AnyToDeviceEvent,
	serde::Raw,
};
use tuwunel_core::{
	Err, Result, err,
	utils::{result::IsErrOr, stream::ReadyExt},
};
use tuwunel_service::Services;

use crate::{ClientIp, Ruma};

#[derive(Default)]
struct Page {
	events: Vec<Raw<AnyToDeviceEvent>>,
	last: Option<u64>,
	more: bool,
}

const MAX_BATCH_EVENTS: usize = 50;

/// # `GET /_matrix/client/../dehydrated_device/{device_id}/events`
///
/// Paginates the events of the dehydrated device.
///
/// `next_batch` is omitted on the last page, as MSC3814 requires.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%client,
	),
)]
pub(crate) async fn get_dehydrated_events_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<Request>,
) -> Result<Response> {
	let since = body.from.as_deref();
	let Page { events, last, more } =
		dehydrated_events(&services, body.sender_user(), &body.device_id, since).await?;

	let next_batch = last
		.filter(|_| more)
		.as_ref()
		.map(ToString::to_string);

	Ok(Response { next_batch, events })
}

/// # `POST /_matrix/client/../dehydrated_device/{device_id}/events`
///
/// Paginates the events of the dehydrated device.
///
/// Older clients still send this earlier unstable form, with the batch token
/// in the body. An empty page ends the events.
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%client,
	),
)]
pub(crate) async fn get_dehydrated_events_by_body_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<ByBodyRequest>,
) -> Result<ByBodyResponse> {
	let since = body.next_batch.as_deref();
	let Page { events, last, .. } =
		dehydrated_events(&services, body.sender_user(), &body.device_id, since).await?;

	let next_batch = last.as_ref().map(ToString::to_string);

	Ok(ByBodyResponse { next_batch, events })
}

async fn dehydrated_events(
	services: &Services,
	sender_user: &UserId,
	device_id: &DeviceId,
	since: Option<&str>,
) -> Result<Page> {
	let existing_id = services
		.users
		.get_dehydrated_device_id(sender_user)
		.await;

	if existing_id
		.as_ref()
		.is_err_or(|existing_id| existing_id != device_id)
	{
		return Err!(Request(Forbidden("Not the dehydrated device_id.")));
	}

	let since = since
		.filter(|since| !since.is_empty())
		.map(str::parse)
		.transpose()
		.map_err(|_| err!(Request(InvalidParam("Unparsable dehydrated device batch token."))))?;

	let page = services
		.users
		.get_to_device_events(sender_user, device_id, since, None)
		.take(MAX_BATCH_EVENTS.saturating_add(1))
		.ready_fold(Page::default(), Page::push)
		.await;

	Ok(page)
}

impl Page {
	fn push(mut self, (count, event): (u64, Raw<AnyToDeviceEvent>)) -> Self {
		if self.events.len() < MAX_BATCH_EVENTS {
			self.events.push(event);
			self.last = Some(count);
		} else {
			self.more = true;
		}

		self
	}
}
