use futures::StreamExt;
use ruma::api::client::sync::sync_events::v5::response;
use tuwunel_core::{self, Result, at};

use super::{Connection, SyncInfo};

#[tracing::instrument(name = "to_device", level = "trace", skip_all, ret)]
pub(super) async fn collect(
	SyncInfo { services, sender_user, sender_device, .. }: SyncInfo<'_>,
	conn: &Connection,
) -> Result<Option<response::ToDevice>> {
	services
		.users
		.remove_to_device_events(sender_user, sender_device, conn.globalsince)
		.await;

	let events: Vec<_> = services
		.users
		.get_to_device_events(sender_user, sender_device, None, Some(conn.next_batch))
		.map(at!(1))
		.collect()
		.await;

	let to_device = events
		.is_empty()
		.eq(&false)
		.then(|| response::ToDevice {
			next_batch: conn.next_batch.to_string().into(),
			events,
		});

	Ok(to_device)
}
