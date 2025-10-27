use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use futures::StreamExt;
use ruma::api::client::dehydrated_device::{
	delete_dehydrated_device::unstable as delete_dehydrated_device,
	get_dehydrated_device::unstable as get_dehydrated_device, get_events::unstable as get_events,
	put_dehydrated_device::unstable as put_dehydrated_device,
};
use tuwunel_core::{Err, Result, at, utils::result::IsErrOr};

use crate::Ruma;

const MAX_BATCH_EVENTS: usize = 50;

/// # `PUT /_matrix/client/../dehydrated_device`
///
/// Creates or overwrites the user's dehydrated device.
#[tracing::instrument(skip_all, fields(%client))]
pub(crate) async fn put_dehydrated_device_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<put_dehydrated_device::Request>,
) -> Result<put_dehydrated_device::Response> {
	let sender_user = body
		.sender_user
		.as_deref()
		.expect("AccessToken authentication required");

	let device_id = body.body.device_id.clone();

	services
		.users
		.set_dehydrated_device(sender_user, body.body)
		.await?;

	Ok(put_dehydrated_device::Response { device_id })
}

/// # `DELETE /_matrix/client/../dehydrated_device`
///
/// Deletes the user's dehydrated device without replacement.
#[tracing::instrument(skip_all, fields(%client))]
pub(crate) async fn delete_dehydrated_device_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<delete_dehydrated_device::Request>,
) -> Result<delete_dehydrated_device::Response> {
	let sender_user = body.sender_user();

	let device_id = services
		.users
		.get_dehydrated_device_id(sender_user)
		.await?;

	services
		.users
		.remove_device(sender_user, &device_id)
		.await;

	Ok(delete_dehydrated_device::Response { device_id })
}

/// # `GET /_matrix/client/../dehydrated_device`
///
/// Gets the user's dehydrated device
#[tracing::instrument(skip_all, fields(%client))]
pub(crate) async fn get_dehydrated_device_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_dehydrated_device::Request>,
) -> Result<get_dehydrated_device::Response> {
	let sender_user = body.sender_user();

	let device = services
		.users
		.get_dehydrated_device(sender_user)
		.await?;

	Ok(get_dehydrated_device::Response {
		device_id: device.device_id,
		device_data: device.device_data,
	})
}

/// # `GET /_matrix/client/../dehydrated_device/{device_id}/events`
///
/// Paginates the events of the dehydrated device.
#[tracing::instrument(skip_all, fields(%client))]
pub(crate) async fn get_dehydrated_events_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
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
