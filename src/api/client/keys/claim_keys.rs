use std::collections::BTreeMap;

use axum::extract::State;
use futures::{FutureExt, StreamExt, future::join};
use ruma::{
	OneTimeKeyAlgorithm, OwnedDeviceId, OwnedOneTimeKeyId, OwnedUserId, ServerName, UserId,
	api::{client::keys::claim_keys, federation},
	encryption::OneTimeKey,
	serde::Raw,
};
use serde_json::json;
use tuwunel_core::{
	Result, debug_warn,
	utils::{
		IterStream,
		stream::{BroadbandExt, ReadyExt},
	},
};
use tuwunel_service::Services;

use super::FailureMap;
use crate::Ruma;

#[derive(Default)]
struct Claims {
	one_time_keys: OneTimeKeyMap,
	failures: FailureMap,
}

type RequestClaims = BTreeMap<OwnedUserId, Algorithms>;
type ServerClaims<'a> = BTreeMap<&'a ServerName, RequestClaims>;
type LocalClaim<'a> = (&'a UserId, &'a Algorithms);
type Algorithms = BTreeMap<OwnedDeviceId, OneTimeKeyAlgorithm>;
type OneTimeKeys = BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>;
type OneTimeKeyMap = BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, OneTimeKeys>>;

/// # `POST /_matrix/client/r0/keys/claim`
///
/// Claims one-time keys
pub(crate) async fn claim_keys_route(
	State(services): State<crate::State>,
	body: Ruma<claim_keys::v3::Request>,
) -> Result<claim_keys::v3::Response> {
	claim_keys_helper(&services, &body.one_time_keys).await
}

pub(crate) async fn claim_keys_helper(
	services: &Services,
	one_time_keys_input: &RequestClaims,
) -> Result<claim_keys::v3::Response> {
	let (local_users, remote_users): (Vec<_>, Vec<_>) = one_time_keys_input
		.iter()
		.map(|(uid, map)| (uid.as_ref(), map))
		.partition(|(user_id, _)| services.globals.user_is_local(user_id));

	let server: ServerClaims<'_> =
		remote_users
			.into_iter()
			.fold(BTreeMap::new(), |mut acc, (user_id, map)| {
				acc.entry(user_id.server_name())
					.or_default()
					.insert(user_id.to_owned(), map.clone());
				acc
			});

	let local = collect_local_one_time_keys(services, &local_users);
	let federation = collect_federation_one_time_keys(services, server);

	let (local, federation) = join(local, federation).await;
	let merged = local.merge(federation);

	Ok(claim_keys::v3::Response {
		failures: merged.failures,
		one_time_keys: merged.one_time_keys,
	})
}

async fn collect_local_one_time_keys(services: &Services, users: &[LocalClaim<'_>]) -> Claims {
	let take_one_time_key = async |(user_id, device_id, algorithm)| {
		services
			.users
			.take_one_time_key(user_id, device_id, algorithm)
			.await
			.ok()
			.map(|key| (device_id.to_owned(), [key].into()))
	};

	let one_time_keys = users
		.iter()
		.copied()
		.stream()
		.broad_then(async |(user_id, requested)| {
			requested
				.iter()
				.stream()
				.map(|(device_id, algorithm)| (user_id, device_id.as_ref(), algorithm))
				.filter_map(take_one_time_key)
				.collect()
				.map(|device_keys| (user_id.to_owned(), device_keys))
				.await
		})
		.collect()
		.await;

	Claims { one_time_keys, ..Default::default() }
}

async fn collect_federation_one_time_keys(
	services: &Services,
	server: ServerClaims<'_>,
) -> Claims {
	server
		.into_iter()
		.stream()
		.broad_then(async |(server, one_time_keys)| {
			let request = federation::keys::claim_keys::v1::Request { one_time_keys };

			match services
				.federation
				.execute(server, request)
				.await
				.inspect_err(
					|e| debug_warn!(%server, "claim_keys federation request failed: {e}"),
				) {
				| Ok(keys) => Claims {
					one_time_keys: keys.one_time_keys,
					failures: Default::default(),
				},
				| Err(_e) => Claims {
					failures: [(server.to_string(), json!({}))].into(),
					..Default::default()
				},
			}
		})
		.ready_fold(Claims::default(), Claims::merge)
		.await
}

impl Claims {
	fn merge(mut self, other: Self) -> Self {
		self.one_time_keys.extend(other.one_time_keys);
		self.failures.extend(other.failures);
		self
	}
}
