use std::collections::BTreeMap;

use futures::StreamExt;
use ruma::{
	OneTimeKeyAlgorithm, OwnedDeviceId, OwnedOneTimeKeyId, UserId,
	api::appservice::{
		Registration,
		keys::{
			claim_keys::unstable::Request as ClaimRequest,
			query_keys::unstable::Request as QueryRequest,
		},
	},
	encryption::{DeviceKeys, OneTimeKey},
	serde::Raw,
};
use tuwunel_core::{
	implement,
	smallvec::SmallVec,
	utils::{IterStream, stream::ReadyExt},
};

type ClaimedKeys = BTreeMap<OwnedDeviceId, BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>>;
type QueriedKeys = BTreeMap<OwnedDeviceId, Raw<DeviceKeys>>;
type Registrations = SmallVec<[Registration; 1]>;

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, one_time_keys))]
pub async fn claim_keys(
	&self,
	user_id: &UserId,
	one_time_keys: &BTreeMap<OwnedDeviceId, OneTimeKeyAlgorithm>,
) -> ClaimedKeys {
	self.registrations_for_user(user_id)
		.await
		.into_iter()
		.stream()
		.filter_map(async |registration| {
			let devices = one_time_keys
				.iter()
				.map(|(device_id, algorithm)| (device_id.clone(), vec![algorithm.clone()]))
				.collect();

			let request = ClaimRequest {
				one_time_keys: [(user_id.to_owned(), devices)].into(),
			};

			self.send_request(registration, request)
				.await
				.ok()
				.flatten()
				.and_then(|mut response| response.one_time_keys.remove(user_id))
		})
		.ready_fold(ClaimedKeys::new(), |claimed, response| {
			response
				.into_iter()
				.fold(claimed, |mut claimed, (device_id, keys)| {
					let Some(algorithm) = one_time_keys.get(&device_id) else {
						return claimed;
					};

					let keys = keys
						.into_iter()
						.filter(|(key_id, _)| key_id.algorithm() == *algorithm)
						.take(1)
						.collect::<BTreeMap<_, _>>();

					if !keys.is_empty() {
						claimed.insert(device_id, keys);
					}

					claimed
				})
		})
		.await
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip(self, devices))]
pub async fn query_keys(&self, user_id: &UserId, devices: &[OwnedDeviceId]) -> QueriedKeys {
	self.registrations_for_user(user_id)
		.await
		.into_iter()
		.stream()
		.filter_map(async |registration| {
			let request = QueryRequest {
				device_keys: [(user_id.to_owned(), devices.to_vec())].into(),
			};

			self.send_request(registration, request)
				.await
				.ok()
				.flatten()
				.and_then(|mut response| response.device_keys.remove(user_id))
		})
		.ready_fold(QueriedKeys::new(), |mut queried, response| {
			queried.extend(response);
			queried
		})
		.await
}

#[implement(super::Service)]
async fn registrations_for_user(&self, user_id: &UserId) -> Registrations {
	self.read()
		.await
		.values()
		.filter(|info| info.is_user_match(user_id))
		.map(|info| info.registration.clone())
		.collect()
}
