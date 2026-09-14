use std::{
	collections::{BTreeMap, BTreeSet},
	str::from_utf8,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::{StreamExt, future::join};
use ruma::{
	OneTimeKeyAlgorithm, OwnedDeviceId, OwnedUserId, UInt, UserId,
	api::appservice::event::push_events::v1::{
		AnyAppserviceToDeviceEvent, DeviceLists, EphemeralData, Request as PushEventsRequest,
	},
	events::AnyTimelineEvent,
	serde::Raw,
};
use serde::Deserialize;
use tuwunel_core::{
	Event, debug_warn, err, implement,
	itertools::Itertools,
	smallvec::SmallVec,
	utils::{
		IterStream, ReadyExt, calculate_hash,
		stream::{BroadbandExt, WidebandExt},
	},
};

use super::SendingResult;
use crate::{
	appservice::RegistrationInfo,
	sending::{Destination, SendingEvent, Service, TAG_PREFIX_LEN},
};

/// The appservice-injected recipient fields of a queued to-device event
/// (MSC4203).
///
/// Parsed to scope MSC3202 one-time-key counts to the addressed devices.
#[derive(Deserialize)]
struct ToDeviceRecipient {
	to_user_id: OwnedUserId,
	to_device_id: OwnedDeviceId,
}

/// The wire pieces of one appservice transaction, accumulated per event.
///
/// `otk_users` and `otk_recipients` are the MSC3202 one-time-key scope: the
/// appservice sender, the namespace-matched PDU senders, and the to-device
/// recipients of the transaction.
#[derive(Default)]
struct Parts {
	pdus: Vec<Raw<AnyTimelineEvent>>,
	edus: Vec<Raw<EphemeralData>>,
	to_device: Vec<Raw<AnyAppserviceToDeviceEvent>>,
	changed: Vec<OwnedUserId>,
	otk_users: BTreeSet<OwnedUserId>,
	otk_recipients: Devices,
}

/// One queued event rendered for the appservice transaction.
///
/// `None` is an event the appservice does not receive, or one that failed to
/// load or decode; the transaction skips it.
enum Part {
	Pdu(Raw<AnyTimelineEvent>, Option<OwnedUserId>),
	Edu(Raw<EphemeralData>),
	ToDevice(Raw<AnyAppserviceToDeviceEvent>, Option<UserDevice>),
	Changed(OwnedUserId),
	None,
}

/// One device of one user.
///
/// The MSC3202 key-count maps are keyed by this pair.
type UserDevice = (OwnedUserId, OwnedDeviceId);

/// MSC3202 `device_one_time_keys_count`: unclaimed one-time-key counts per
/// algorithm, keyed by user then device.
///
/// Matches the ruma request field type.
type OtkCounts =
	BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, BTreeMap<OneTimeKeyAlgorithm, UInt>>>;

/// MSC3202 `device_unused_fallback_key_types`: algorithms with an unused
/// fallback key, keyed by user then device.
///
/// Matches the ruma request field type.
type FallbackTypes = BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Vec<OneTimeKeyAlgorithm>>>;

/// The MSC3202-interesting devices of one transaction: the appservice
/// sender's plus matched PDU senders' devices and the to-device recipients.
///
/// Sorted and deduplicated before the per-device key lookups fan out.
type Devices = SmallVec<[UserDevice; 1]>;

#[implement(Service)]
#[tracing::instrument(
	name = "appservice",
	level = "debug",
	skip(self, events),
	fields(
		events = %events.len(),
	),
)]
pub(super) async fn send_events_dest_appservice(
	&self,
	id: String,
	events: Vec<SendingEvent>,
) -> SendingResult {
	let Some(info) = self
		.services
		.appservice
		.get_registration_info(&id)
		.await
	else {
		//TODO: appservice queue cleanup.
		return Err((
			Destination::Appservice(id.clone()),
			err!(Database(debug_warn!(?id, "Missing appservice registration"))),
		));
	};

	let msc3202 = info.registration.msc3202_transaction_extensions;
	let parts = Parts {
		otk_users: msc3202
			.then(|| info.sender.clone())
			.into_iter()
			.collect(),
		..Default::default()
	};

	let Parts {
		pdus,
		edus,
		to_device,
		changed,
		otk_users,
		otk_recipients,
	} = events
		.iter()
		.stream()
		.wide_then(async |event| self.txn_part(&info, msc3202, event).await)
		.ready_fold(parts, Parts::merge)
		.await;

	let txn_hash = calculate_hash(events.iter().filter_map(|e| match e {
		| SendingEvent::Edu(b)
		| SendingEvent::ToDevice(b)
		| SendingEvent::DeviceListChanged(b) => Some(b.as_ref()),
		| SendingEvent::Pdu(b) => Some(b.as_ref()),
		| SendingEvent::BadgeRefresh | SendingEvent::Flush => None,
	}));

	let (device_lists, device_one_time_keys_count, device_unused_fallback_key_types) = if msc3202
	{
		let changed = changed
			.into_iter()
			.sorted_unstable()
			.dedup()
			.collect();

		let (counts, fallbacks) = self
			.msc3202_key_counts(otk_users, otk_recipients)
			.await;

		(DeviceLists { changed, left: Vec::new() }, counts, fallbacks)
	} else {
		(DeviceLists::new(), OtkCounts::new(), FallbackTypes::new())
	};

	if pdus.is_empty()
		&& edus.is_empty()
		&& to_device.is_empty()
		&& device_lists.is_empty()
		&& device_one_time_keys_count.is_empty()
		&& device_unused_fallback_key_types.is_empty()
	{
		return Ok(Destination::Appservice(id));
	}

	let request = PushEventsRequest {
		txn_id: URL_SAFE_NO_PAD.encode(txn_hash).into(),
		events: pdus,
		ephemeral: edus,
		to_device,
		device_lists,
		device_one_time_keys_count,
		device_unused_fallback_key_types,
	};

	match self
		.services
		.appservice
		.send_request(info.registration, request)
		.await
	{
		| Ok(_) => Ok(Destination::Appservice(id)),
		| Err(e) => Err((Destination::Appservice(id), e)),
	}
}

#[implement(Service)]
async fn txn_part(&self, info: &RegistrationInfo, msc3202: bool, event: &SendingEvent) -> Part {
	match event {
		| SendingEvent::Pdu(pdu_id) => {
			let Ok(pdu) = self
				.services
				.timeline
				.get_pdu_from_id(pdu_id)
				.await
			else {
				return Part::None;
			};

			let sender =
				(msc3202 && info.is_user_match(pdu.sender())).then(|| pdu.sender().to_owned());

			Part::Pdu(pdu.to_format(), sender)
		},
		| SendingEvent::Edu(edu) => {
			if !info.registration.receive_ephemeral {
				return Part::None;
			}

			serde_json::from_slice::<EphemeralData>(edu)
				.and_then(|edu| Raw::new(&edu))
				.map_or(Part::None, Part::Edu)
		},
		| SendingEvent::ToDevice(buf) => {
			let Some(bytes) = buf.get(TAG_PREFIX_LEN..) else {
				debug_warn!("skipping malformed queued to-device event");
				return Part::None;
			};

			let Ok(raw) = serde_json::from_slice(bytes) else {
				debug_warn!("skipping malformed queued to-device event");
				return Part::None;
			};

			let recipient = msc3202
				.then(|| serde_json::from_slice(bytes).ok())
				.flatten()
				.map(|ToDeviceRecipient { to_user_id, to_device_id }| (to_user_id, to_device_id));

			Part::ToDevice(raw, recipient)
		},
		| SendingEvent::DeviceListChanged(buf) => {
			if msc3202
				&& let Some(bytes) = buf.get(TAG_PREFIX_LEN..)
				&& let Ok(user) = from_utf8(bytes)
				&& let Ok(user_id) = UserId::parse(user)
			{
				return Part::Changed(user_id);
			}

			Part::None
		},
		| SendingEvent::BadgeRefresh | SendingEvent::Flush => Part::None,
	}
}

impl Parts {
	fn merge(mut self, part: Part) -> Self {
		match part {
			| Part::Pdu(pdu, sender) => {
				self.pdus.push(pdu);
				self.otk_users.extend(sender);
			},
			| Part::Edu(edu) => self.edus.push(edu),
			| Part::ToDevice(raw, recipient) => {
				self.to_device.push(raw);
				self.otk_recipients.extend(recipient);
			},
			| Part::Changed(user_id) => self.changed.push(user_id),
			| Part::None => {},
		}

		self
	}
}

/// MSC3202 one-time-key counts and unused fallback key types over every
/// device of `users` plus the specific `recipients`.
///
/// Recomputed per build rather than snapshotted, so a retry ships fresh
/// counts.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		users = %users.len(),
		recipients = %recipients.len(),
	),
)]
async fn msc3202_key_counts(
	&self,
	users: BTreeSet<OwnedUserId>,
	recipients: Devices,
) -> (OtkCounts, FallbackTypes) {
	let devices: Devices = users
		.iter()
		.stream()
		.flat_map(|user_id| {
			self.services
				.users
				.all_device_ids(user_id)
				.map(move |device_id| (user_id.to_owned(), device_id.to_owned()))
		})
		.chain(recipients.into_iter().stream())
		.collect()
		.await;

	dedup(devices)
		.into_iter()
		.stream()
		.broad_then(async |(user_id, device_id): UserDevice| {
			let counts = self
				.services
				.users
				.count_one_time_keys(&user_id, &device_id);

			let fallbacks = self
				.services
				.users
				.unused_fallback_key_algorithms(&user_id, &device_id)
				.collect();

			let (counts, fallbacks) = join(counts, fallbacks).await;

			(user_id, device_id, counts, fallbacks)
		})
		.ready_fold(
			(OtkCounts::new(), FallbackTypes::new()),
			|(mut counts, mut fallbacks), (user_id, device_id, otk, fallback)| {
				counts
					.entry(user_id.clone())
					.or_default()
					.insert(device_id.clone(), otk);

				fallbacks
					.entry(user_id)
					.or_default()
					.insert(device_id, fallback);

				(counts, fallbacks)
			},
		)
		.await
}

fn dedup(mut devices: Devices) -> Devices {
	devices.sort_unstable();
	devices.dedup();

	devices
}
