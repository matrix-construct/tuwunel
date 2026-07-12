use std::{collections::BTreeMap, iter::once};

use axum::extract::State;
use futures::StreamExt;
use ruma::{
	OwnedDeviceId,
	api::{
		client::to_device::send_event_to_device,
		error::ErrorKind,
		federation::{self, transactions::edu::DirectDeviceContent},
	},
	to_device::DeviceIdOrAllDevices,
};
use tuwunel_core::{
	Error, Result,
	smallvec::SmallVec,
	utils::{ReadyExt, result::LogErr},
};
use tuwunel_service::sending::EduBuf;

use crate::Ruma;

/// Recipient devices of one `AllDevices` to-device send paired with their
/// inbox counts.
type Deliveries = SmallVec<[(OwnedDeviceId, u64); 1]>;

/// # `PUT /_matrix/client/r0/sendToDevice/{eventType}/{txnId}`
///
/// Send a to-device event to a set of client devices.
pub(crate) async fn send_event_to_device_route(
	State(services): State<crate::State>,
	body: Ruma<send_event_to_device::v3::Request>,
) -> Result<send_event_to_device::v3::Response> {
	let sender_user = body.sender_user();
	let sender_device = body.sender_device.as_deref();

	// Check if this is a new transaction id
	if services
		.transaction_ids
		.existing_txnid(sender_user, sender_device, &body.txn_id)
		.await
		.is_ok()
	{
		return Ok(send_event_to_device::v3::Response {});
	}

	for (target_user_id, map) in &body.messages {
		for (target_device_id_maybe, event) in map {
			if !services.globals.user_is_local(target_user_id) {
				let mut map = BTreeMap::new();
				map.insert(target_device_id_maybe.clone(), event.clone());
				let mut messages = BTreeMap::new();
				messages.insert(target_user_id.clone(), map);

				let mut buf = EduBuf::new();
				serde_json::to_writer(
					&mut buf,
					&federation::transactions::edu::Edu::DirectToDevice(DirectDeviceContent {
						sender: sender_user.to_owned(),
						ev_type: body.event_type.clone(),
						message_id: services.globals.next_count().to_string().into(),
						messages,
					}),
				)
				.expect("DirectToDevice EDU can be serialized");

				services
					.sending
					.send_edu_server(target_user_id.server_name(), buf)?;

				continue;
			}

			let event_type = &body.event_type.to_string();

			let event = event
				.deserialize_as()
				.map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Event is invalid"))?;

			match target_device_id_maybe {
				| DeviceIdOrAllDevices::DeviceId(target_device_id) => {
					let count = services.users.add_to_device_event(
						sender_user,
						target_user_id,
						target_device_id,
						event_type,
						&event,
					);

					services
						.sending
						.send_to_device_appservices(
							sender_user,
							target_user_id,
							once((&**target_device_id, count)),
							event_type,
							&event,
						)
						.await
						.log_err()
						.ok();
				},

				| DeviceIdOrAllDevices::AllDevices => {
					let interested = services
						.appservice
						.is_interested_in_user(target_user_id)
						.await;

					let deliveries: Deliveries = services
						.users
						.all_device_ids(target_user_id)
						.map(|target_device_id| {
							let count = services.users.add_to_device_event(
								sender_user,
								target_user_id,
								target_device_id,
								event_type,
								&event,
							);

							(target_device_id, count)
						})
						.ready_filter_map(|(target_device_id, count)| {
							interested.then(|| (target_device_id.to_owned(), count))
						})
						.collect()
						.await;

					if !deliveries.is_empty() {
						services
							.sending
							.send_to_device_appservices(
								sender_user,
								target_user_id,
								deliveries
									.iter()
									.map(|(device_id, count)| (&**device_id, *count)),
								event_type,
								&event,
							)
							.await
							.log_err()
							.ok();
					}
				},
			}
		}
	}

	// Save transaction id with empty data
	services
		.transaction_ids
		.add_txnid(sender_user, sender_device, &body.txn_id, &[]);

	Ok(send_event_to_device::v3::Response {})
}
