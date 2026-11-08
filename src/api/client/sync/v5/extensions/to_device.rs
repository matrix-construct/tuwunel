use futures::StreamExt;
use ruma::{
	DeviceId, UserId,
	api::client::sync::sync_events::v5::{request::ToDevice, response},
};
use tuwunel_core::{
	Err, Result, err,
	utils::{BoolExt, math::usize_from_ruma_bounded, stream::ReadyExt},
};
use tuwunel_service::Services;

use super::{Connection, SyncInfo};

const DEFAULT_LIMIT: usize = 100;

pub(in crate::sync::v5) fn check(request: &ToDevice) -> Result {
	if request
		.limit
		.is_some_and(|limit| u64::from(limit) == 0)
	{
		return Err!(Request(BadJson("To-device limit must be positive.")));
	}

	since(request).map(|_| ())
}

#[tracing::instrument(level = "trace", skip_all)]
pub(in crate::sync::v5) async fn acknowledge(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	conn: &Connection,
) -> Result {
	let Some(sender_device) =
		sender_device.filter(|_| conn.extensions.to_device.enabled.unwrap_or(false))
	else {
		return Ok(());
	};

	// MSC4538 acknowledgements use the extension's token independently of pos.
	let Some(since) = since(&conn.extensions.to_device)? else {
		return Ok(());
	};

	// MSC4538:59-60 deviation: an unissued token returns and deletes nothing;
	// clients treat 400 as permanent.
	if since > conn.next_batch {
		return Ok(());
	}

	services
		.users
		.remove_to_device_events(sender_user, sender_device, since)
		.await;

	Ok(())
}

#[tracing::instrument(name = "to_device", level = "trace", skip_all, ret)]
pub(super) async fn collect(
	SyncInfo { services, sender_user, sender_device, .. }: SyncInfo<'_>,
	conn: &Connection,
) -> Result<Option<response::ToDevice>> {
	let Some(sender_device) = sender_device else {
		return Ok(None);
	};

	let since = since(&conn.extensions.to_device)?;

	let (next_batch, events) = services
		.users
		.get_to_device_events(sender_user, sender_device, since, Some(conn.next_batch))
		.take(limit(&conn.extensions.to_device))
		.ready_fold((0, Vec::new()), |(_, mut events), (count, event)| {
			events.push(event);
			(count, events)
		})
		.await;

	let to_device = events
		.is_empty()
		.is_false()
		.then(|| response::ToDevice {
			next_batch: next_batch.to_string().into(),
			events,
		});

	Ok(to_device)
}

pub(super) fn limit(config: &ToDevice) -> usize {
	config
		.limit
		.map_or(DEFAULT_LIMIT, |limit| usize_from_ruma_bounded(limit, usize::MAX, usize::MAX))
}

#[tracing::instrument(level = "trace", skip_all)]
pub(in crate::sync::v5) async fn pending(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	conn: &Connection,
) -> Result<bool> {
	let Some(sender_device) =
		sender_device.filter(|_| conn.extensions.to_device.enabled.unwrap_or(false))
	else {
		return Ok(false);
	};

	let pending = services
		.users
		.get_to_device_events(
			sender_user,
			sender_device,
			since(&conn.extensions.to_device)?,
			Some(conn.next_batch),
		)
		.ready_any(|_| true)
		.await;

	Ok(pending)
}

fn since(config: &ToDevice) -> Result<Option<u64>> {
	config
		.since
		.as_deref()
		.map(str::parse)
		.transpose()
		.map_err(|_| err!(Request(InvalidParam("Unparsable to-device since."))))
}
