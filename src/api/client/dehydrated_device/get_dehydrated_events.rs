use axum::extract::State;
use futures::StreamExt;
use ruma::api::client::dehydrated_device::get_events::unstable as get_events;
use tuwunel_core::{Err, Result, at, utils::result::IsErrOr};

use crate::{ClientIp, Ruma};

const MAX_BATCH_EVENTS: usize = 50;

/// # `POST /_matrix/client/../dehydrated_device/{device_id}/events`
///
/// Paginates the events of the dehydrated device.
#[tracing::instrument(skip_all, fields(%client))]
pub(crate) async fn get_dehydrated_events_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	body: Ruma<get_events::Request>,
) -> Result<get_events::Response> {
	let sender_user = body.sender_user();

	let device_id = &body.body.device_id;
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

	let since: Option<u64> = body
		.body
		.next_batch
		.as_deref()
		.map(str::parse)
		.transpose()?;

	let mut next_batch: Option<u64> = None;
	let events = services
		.users
		.get_to_device_events(sender_user, device_id, since, None)
		.take(MAX_BATCH_EVENTS)
		.inspect(|&(count, _)| {
			next_batch.replace(count);
		})
		.map(at!(1))
		.collect()
		.await;

	Ok(get_events::Response {
		events,
		next_batch: next_batch.as_ref().map(ToString::to_string),
	})
}
