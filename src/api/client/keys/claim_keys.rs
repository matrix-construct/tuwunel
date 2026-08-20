use std::collections::BTreeMap;

use axum::extract::State;
use futures::{StreamExt, future::join};
use ruma::{
	OneTimeKeyAlgorithm, OwnedDeviceId, OwnedOneTimeKeyId, OwnedUserId, ServerName, UserId,
	api::{
		client::keys::claim_keys,
		federation::keys::claim_keys::v1::{
			Request as FederationRequest, Response as FederationResponse,
		},
	},
	encryption::OneTimeKey,
	serde::Raw,
};
use tuwunel_core::{
	Result,
	utils::{BoolExt, IterStream, stream::BroadbandExt},
};
use tuwunel_service::{
	Services,
	federation::feds::{OutcomeExt, fanout_with},
};

use super::{FailureMap, federation_failures, federation_opts};
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
	let one_time_keys = users
		.iter()
		.copied()
		.stream()
		.broad_filter_map(async |(user_id, requested)| {
			let (mut device_keys, mut needed) = requested
				.iter()
				.stream()
				.fold(
					(BTreeMap::new(), BTreeMap::new()),
					async |(mut device_keys, mut needed), (device_id, algorithm)| {
						match services
							.users
							.take_one_time_key(user_id, device_id, algorithm)
							.await
						{
							| Ok(key) => {
								device_keys.insert(device_id.clone(), [key].into());
							},
							| Err(_) => {
								needed.insert(device_id.clone(), algorithm.clone());
							},
						}

						(device_keys, needed)
					},
				)
				.await;

			// MSC3983: claim from appservices before marking local fallback keys used.
			let claimed = needed
				.is_empty()
				.is_false()
				.then_async(|| services.appservice.claim_keys(user_id, &needed))
				.await
				.unwrap_or_default();

			for (device_id, keys) in claimed {
				needed.remove(&device_id);
				device_keys.insert(device_id, keys);
			}

			let device_keys = needed
				.into_iter()
				.stream()
				.fold(device_keys, async |mut device_keys, (device_id, algorithm)| {
					if let Ok(key) = services
						.users
						.take_fallback_key(user_id, &device_id, &algorithm)
						.await
					{
						device_keys.insert(device_id, [key].into());
					}

					device_keys
				})
				.await;

			// Omit a depleted user entirely; Synapse returns no entry, not an empty map.
			(!device_keys.is_empty()).then(|| (user_id.to_owned(), device_keys))
		})
		.collect()
		.await;

	Claims { one_time_keys, ..Default::default() }
}

async fn collect_federation_one_time_keys(
	services: &Services,
	server: ServerClaims<'_>,
) -> Claims {
	let requests = server
		.into_iter()
		.stream()
		.map(|(server, one_time_keys)| (server.to_owned(), FederationRequest { one_time_keys }));

	let outcomes = fanout_with(
		requests,
		async |server, request| {
			services
				.federation
				.execute_keys(&server, request)
				.await
		},
		federation_opts(services),
	);

	let (claims, faults) = outcomes
		.merge(Claims::default(), |mut claims, response: FederationResponse| {
			claims
				.one_time_keys
				.extend(response.one_time_keys);

			claims
		})
		.await;

	Claims {
		failures: federation_failures("claim_keys", faults).collect(),
		..claims
	}
}

impl Claims {
	fn merge(mut self, other: Self) -> Self {
		self.one_time_keys.extend(other.one_time_keys);
		self.failures.extend(other.failures);
		self
	}
}
