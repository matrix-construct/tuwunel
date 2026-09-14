use std::iter::once;

use futures::StreamExt;
use ruma::{DeviceId, OwnedRoomId, UserId};
use serde::Serialize;
use tuwunel_core::{Result, implement, utils::stream::BroadbandExt};

use super::{Destination, EduBuf, SendingEvent, Service, TAG_DEVICE_LIST_CHANGED, TAG_TO_DEVICE};
use crate::appservice::RegistrationInfo;

/// One recipient device of a to-device event paired with its inbox count.
///
/// The count uniquifies the appservice transaction hash.
type Delivery<'a> = (&'a DeviceId, u64);

/// Wire shape of one `de.sorunome.msc2409.to_device` entry (MSC4203).
///
/// The stored to-device event is flattened with the recipient's identifiers.
/// The ruma `AnyAppserviceToDeviceEvent` deliberately has no `Serialize`, so
/// the send side writes this local struct.
#[derive(Serialize)]
struct AsToDeviceEvent<'a> {
	#[serde(rename = "type")]
	kind: &'a str,
	sender: &'a UserId,
	content: &'a serde_json::Value,
	to_user_id: &'a UserId,
	to_device_id: &'a DeviceId,
}

/// Queue stored to-device events for delivery to interested appservices
/// (MSC4203).
///
/// `deliveries` are the concrete recipient devices already written to the
/// inbox, after `AllDevices` expansion. Each delivery becomes one queue row
/// per interested appservice, written under one cork; with no interested
/// appservice nothing is serialized.
#[implement(Service)]
#[tracing::instrument(
	skip(self, deliveries, content),
	level = "debug",
	fields(
		%target_user,
	),
)]
pub async fn send_to_device_appservices<'a, I>(
	&self,
	sender: &'a UserId,
	target_user: &'a UserId,
	deliveries: I,
	event_type: &'a str,
	content: &'a serde_json::Value,
) -> Result
where
	I: Iterator<Item = Delivery<'a>> + Send + 'a,
{
	let registrations = self.services.appservice.read().await;
	let interested = || {
		registrations
			.values()
			.filter(|info| info.is_user_match(target_user))
	};

	// Serialize once, and only when an appservice will receive the events.
	if interested().next().is_none() {
		return Ok(());
	}

	let _cork = self.db.db.cork();

	to_device_payloads(sender, target_user, deliveries, event_type, content)
		.flat_map(|buf| interested().map(move |info| (info, buf.clone())))
		.try_for_each(|(info, buf)| {
			let dest = Destination::Appservice(info.registration.id.clone());

			self.queue_and_dispatch(dest, SendingEvent::ToDevice(buf))
		})
}

/// Queue a `device_lists.changed` marker (MSC3202) for delivery to
/// appservices that opted into transaction extensions and are interested
/// in `user_id`.
///
/// The caller passes the count it already allocated so the marker uniquifies
/// the transaction hash.
#[implement(Service)]
#[tracing::instrument(
	skip(self),
	level = "debug",
	fields(
		%user_id,
	),
)]
pub async fn send_device_list_appservices(&self, user_id: &UserId, count: u64) -> Result {
	let registrations = self.services.appservice.read().await;
	let extended = || {
		registrations
			.values()
			.filter(|info| info.registration.msc3202_transaction_extensions)
	};

	if extended().next().is_none() {
		return Ok(());
	}

	let payload = device_list_payload(user_id, count);
	let _cork = self.db.db.cork();

	for info in extended() {
		if !info.is_user_match(user_id) && !self.shares_device_list_room(user_id, info).await {
			continue;
		}

		let dest = Destination::Appservice(info.registration.id.clone());

		self.queue_and_dispatch(dest, SendingEvent::DeviceListChanged(payload.clone()))?;
	}

	Ok(())
}

/// Whether `user_id` shares a device-list-interesting room with `info`.
///
/// A joined room the appservice participates in counts when it is encrypted,
/// or unconditionally when `device_key_update_encrypted_rooms_only` is off.
#[implement(Service)]
async fn shares_device_list_room(&self, user_id: &UserId, info: &RegistrationInfo) -> bool {
	let update_all_rooms = !self
		.services
		.config
		.device_key_update_encrypted_rooms_only;

	self.services
		.state_cache
		.rooms_joined(user_id)
		.map(ToOwned::to_owned)
		.broad_any(async |room_id: OwnedRoomId| {
			if !update_all_rooms
				&& !self
					.services
					.state_accessor
					.is_encrypted_room(&room_id)
					.await
			{
				return false;
			}

			self.services
				.state_cache
				.appservice_in_room(&room_id, info)
				.await
		})
		.await
}

fn to_device_payloads<'a>(
	sender: &'a UserId,
	target_user: &'a UserId,
	deliveries: impl Iterator<Item = Delivery<'a>> + 'a,
	event_type: &'a str,
	content: &'a serde_json::Value,
) -> impl Iterator<Item = EduBuf> + 'a {
	deliveries.map(move |(to_device_id, count)| {
		let event = AsToDeviceEvent {
			kind: event_type,
			sender,
			content,
			to_user_id: target_user,
			to_device_id,
		};

		tagged_json(TAG_TO_DEVICE, count, &event)
	})
}

/// A tagged queue row value whose body is `value` serialized as JSON.
///
/// The body is written straight into the inline buffer after the prefix.
fn tagged_json(tag: u8, count: u64, value: &impl Serialize) -> EduBuf {
	let mut buf = tag_prefix(tag, count); // serde_json::to_writer out-param

	serde_json::to_writer(&mut buf, value).expect("tagged queue row value serializes");
	buf
}

fn device_list_payload(user_id: &UserId, count: u64) -> EduBuf {
	let mut buf = tag_prefix(TAG_DEVICE_LIST_CHANGED, count);

	buf.extend_from_slice(user_id.as_bytes());
	buf
}

/// The `[tag][count]` head every tagged queue row value starts with.
///
/// The count is written big-endian so the prefix is `TAG_PREFIX_LEN` bytes.
fn tag_prefix(tag: u8, count: u64) -> EduBuf { once(tag).chain(count.to_be_bytes()).collect() }
