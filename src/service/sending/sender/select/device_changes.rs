use std::{
	collections::{BTreeMap, BTreeSet},
	sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use futures::{StreamExt, future::join};
use ruma::{
	DeviceId, OwnedUserId, ServerName, UserId,
	api::federation::transactions::edu::{DeviceListUpdateContent, Edu, SigningKeyUpdateContent},
	device_id,
};
use tuwunel_core::{
	implement,
	itertools::Itertools,
	smallstr::SmallString,
	smallvec::SmallVec,
	utils::{BoolExt, IterStream, ReadyExt, stream::WidebandExt},
};

use super::{Selected, edu_buf};
use crate::{
	sending::{EduBuf, Service},
	users::{DeviceListChange, DeviceListRecord},
};

type Pending = BTreeMap<OwnedUserId, BTreeSet<u64>>;
type Records = SmallVec<[DeviceListRecord; 1]>;

enum Plan<D> {
	Resync(u64),
	Deltas {
		signing_key_update: bool,
		deltas: D,
	},
}

#[derive(Debug, Eq, PartialEq)]
struct Delta<'a> {
	device_id: &'a DeviceId,
	stream_id: u64,
	prev_id: Option<u64>,
	deleted: bool,
}

// Above ten distinct devices, a snapshot costs less than individual updates.
const K: usize = 10;

/// Select device-list deltas and signing-key updates for local users.
///
/// Legacy records and windows exceeding the device limit force a snapshot resync.
#[implement(Service)]
#[tracing::instrument(
	name = "device_changes",
	level = "trace",
	skip(self, server_name, max_edu_count, events_len)
)]
pub(super) async fn select_edus_device_changes(
	&self,
	server_name: &ServerName,
	since: (u64, u64),
	max_edu_count: &AtomicU64,
	events_len: &AtomicUsize,
) -> Selected {
	let pending = self
		.services
		.state_cache
		.server_rooms(server_name)
		.map(ToOwned::to_owned)
		.fold(Pending::new(), async |pending, room_id| {
			self.services
				.users
				.room_keys_changed(&room_id, since.0, Some(since.1))
				.ready_filter_map(|(user_id, count)| {
					if !self.services.globals.user_is_local(user_id) {
						return None;
					}

					debug_assert!(count <= since.1, "exceeds upper-bound");
					max_edu_count.fetch_max(count, Ordering::Relaxed);

					Some((user_id.to_owned(), count))
				})
				.ready_fold(pending, |mut pending, (user_id, count)| {
					pending.entry(user_id).or_default().insert(count);
					pending
				})
				.await
		})
		.await;

	pending
		.into_iter()
		.stream()
		.fold(Selected::default(), async |selected, (user_id, counts)| {
			self.select_user_devices(selected, &user_id, counts, server_name, events_len)
				.await
		})
		.await
}

#[implement(Service)]
async fn select_user_devices(
	&self,
	selected: Selected,
	user_id: &UserId,
	counts: BTreeSet<u64>,
	server_name: &ServerName,
	events_len: &AtomicUsize,
) -> Selected {
	let records: Records = counts
		.into_iter()
		.stream()
		.wide_then(async |count| {
			self.services
				.users
				.device_list_change(count)
				.await
				.unwrap_or(DeviceListRecord {
					change: DeviceListChange::Resync,
					stream_id: 0,
				})
		})
		.collect()
		.await;

	let current_stream_id = self
		.services
		.users
		.get_devicelist_version(user_id)
		.await
		.unwrap_or(0);

	let (signing_key_update, deltas) = match plan_device_list_edus(&records, current_stream_id) {
		| Plan::Deltas { signing_key_update, deltas } => (signing_key_update, deltas),
		| Plan::Resync(stream_id) => {
			return push(selected, device_list_edu(user_id, stream_id), events_len);
		},
	};

	let signing = signing_key_update
		.then_async(|| self.signing_key_edu(user_id, server_name))
		.await
		.flatten();

	let selected = signing
		.into_iter()
		.fold(selected, |selected, edu| push(selected, edu, events_len));

	deltas
		.stream()
		.fold(selected, async |selected, delta| {
			let edu = self.device_delta_edu(user_id, delta).await;

			// Current content and the maximum S keep ordered replay convergent.
			push(selected, edu, events_len)
		})
		.await
}

fn plan_device_list_edus(
	records: &[DeviceListRecord],
	current_stream_id: u64,
) -> Plan<impl Iterator<Item = Delta<'_>>> {
	if records
		.iter()
		.any(|record| matches!(record.change, DeviceListChange::Resync))
	{
		return Plan::Resync(current_stream_id);
	}

	let latest: BTreeMap<_, _> = records
		.iter()
		.filter_map(record_delta)
		.map(|delta| (delta.device_id, delta))
		.collect();

	if latest.len() > K {
		return Plan::Resync(current_stream_id);
	}

	let anchor = records
		.iter()
		.filter_map(record_delta)
		.map(|delta| delta.stream_id)
		.min()
		.unwrap_or(0)
		.saturating_sub(1);

	let deltas = latest
		.into_values()
		.sorted_unstable_by_key(|delta| delta.stream_id)
		.scan(anchor, |anchor, delta| {
			let prev_id = u64::ne(anchor, &0).then_some(*anchor);

			*anchor = delta.stream_id;
			Some(Delta { prev_id, ..delta })
		});

	let signing_key_update = records
		.iter()
		.any(|record| matches!(record.change, DeviceListChange::CrossSigning));

	Plan::Deltas { signing_key_update, deltas }
}

fn record_delta(record: &DeviceListRecord) -> Option<Delta<'_>> {
	let (device_id, deleted) = match &record.change {
		| DeviceListChange::CrossSigning | DeviceListChange::Resync => return None,
		| DeviceListChange::Device(device_id) => (device_id.as_ref(), false),
		| DeviceListChange::Deleted(device_id) => (device_id.as_ref(), true),
	};

	Some(Delta {
		device_id,
		stream_id: record.stream_id,
		prev_id: None,
		deleted,
	})
}

fn device_list_edu(user_id: &UserId, stream_id: u64) -> EduBuf {
	edu_buf(&Edu::DeviceListUpdate(DeviceListUpdateContent {
		user_id: user_id.to_owned(),
		device_id: device_id!("placeholder").to_owned(),
		device_display_name: Some("Placeholder".to_owned()),
		stream_id: stream_id.try_into().unwrap_or_default(),
		prev_id: Vec::new(),
		deleted: None,
		keys: None,
	}))
}

fn push(mut selected: Selected, edu: EduBuf, events_len: &AtomicUsize) -> Selected {
	selected.push(edu, events_len);
	selected
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
async fn signing_key_edu(&self, user_id: &UserId, server_name: &ServerName) -> Option<EduBuf> {
	let allowed = |user: &UserId| user.server_name() == server_name;
	let (master_key, self_signing_key) = join(
		self.services
			.users
			.get_master_key(None, user_id, &allowed),
		self.services
			.users
			.get_self_signing_key(None, user_id, &allowed),
	)
	.await;

	let (master_key, self_signing_key) = (master_key.ok(), self_signing_key.ok());

	(master_key.is_some() || self_signing_key.is_some()).then(|| {
		edu_buf(&Edu::SigningKeyUpdate(SigningKeyUpdateContent {
			user_id: user_id.to_owned(),
			master_key,
			self_signing_key,
		}))
	})
}

#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
async fn device_delta_edu(&self, user_id: &UserId, delta: Delta<'_>) -> EduBuf {
	let (keys, device_display_name) = if delta.deleted {
		(None, None)
	} else {
		let metadata = self
			.server
			.config
			.allow_device_name_federation
			.then_async(|| {
				self.services
					.users
					.get_device_metadata(user_id, delta.device_id)
			});

		let (keys, metadata) = join(
			self.services
				.users
				.get_device_keys(user_id, delta.device_id),
			metadata,
		)
		.await;

		let display_name = metadata
			.and_then(Result::ok)
			.and_then(|device| device.display_name)
			.map(SmallString::into_string)
			.or_else(|| Some(delta.device_id.as_str().into()));

		(keys.ok(), display_name)
	};

	let prev_id = delta
		.prev_id
		.into_iter()
		.map(|id| id.try_into().unwrap_or_default())
		.collect();

	edu_buf(&Edu::DeviceListUpdate(DeviceListUpdateContent {
		user_id: user_id.to_owned(),
		device_id: delta.device_id.to_owned(),
		device_display_name,
		stream_id: delta.stream_id.try_into().unwrap_or_default(),
		prev_id,
		deleted: delta.deleted.then_some(true),
		keys,
	}))
}

#[cfg(test)]
mod tests {
	use DeviceListChange::{CrossSigning, Deleted, Device, Resync};

	use super::{DeviceListChange, DeviceListRecord, K, Plan, plan_device_list_edus};

	#[test]
	fn plans_dense_chains() {
		let cases = [
			(vec![(Device("X"), 5)], Some((false, vec![("X", 5, Some(4), false)]))),
			(
				vec![(Device("X"), 5), (Device("Y"), 6), (Device("X"), 7)],
				Some((false, vec![("Y", 6, Some(4), false), ("X", 7, Some(6), false)])),
			),
			(
				vec![(Device("X"), 5), (Deleted("X"), 6)],
				Some((false, vec![("X", 6, Some(4), true)])),
			),
			(vec![(Resync, 5)], None),
			(vec![(CrossSigning, 4)], Some((true, vec![]))),
			(
				vec![(CrossSigning, 4), (Device("X"), 5)],
				Some((true, vec![("X", 5, Some(4), false)])),
			),
			(vec![(Device("X"), 1)], Some((false, vec![("X", 1, None, false)]))),
		];

		for (input, expected) in cases {
			let records: Vec<_> = input
				.into_iter()
				.map(|(change, stream_id)| {
					let change = match change {
						| Resync => Resync,
						| CrossSigning => CrossSigning,
						| Device(id) => Device(id.into()),
						| Deleted(id) => Deleted(id.into()),
					};

					DeviceListRecord { change, stream_id }
				})
				.collect();

			let actual = match plan_device_list_edus(&records, 99) {
				| Plan::Resync(stream_id) => {
					assert_eq!(stream_id, 99);
					None
				},
				| Plan::Deltas { signing_key_update, deltas } => {
					let deltas = deltas
						.map(|d| (d.device_id.as_str(), d.stream_id, d.prev_id, d.deleted))
						.collect();

					Some((signing_key_update, deltas))
				},
			};

			assert_eq!(actual, expected, "{records:?}");
		}
	}

	#[test]
	fn too_many_devices_resync() {
		let records: Vec<_> = (0..=K)
			.map(|id| DeviceListRecord {
				change: Device(id.to_string().into()),
				stream_id: u64::try_from(id).unwrap() + 1,
			})
			.collect();

		assert!(matches!(plan_device_list_edus(&records[..K], 99), Plan::Deltas { .. }));
		assert!(matches!(plan_device_list_edus(&records, 99), Plan::Resync(99)));
	}
}
