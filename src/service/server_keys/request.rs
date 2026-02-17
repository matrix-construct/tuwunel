use std::{collections::BTreeMap, convert::identity, fmt::Debug};

use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	OwnedServerName, OwnedServerSigningKeyId, ServerName, ServerSigningKeyId,
	api::federation::discovery::{
		ServerSigningKeys, get_remote_server_keys,
		get_remote_server_keys_batch::{self, v2::QueryCriteria},
		get_server_keys,
	},
};
use tuwunel_core::{
	Err, Result, error, implement, info, trace,
	utils::stream::{IterStream, ReadyExt, TryBroadbandExt, TryReadyExt},
};

#[implement(super::Service)]
pub(super) async fn batch_notary_request<'a, S, K>(
	&self,
	notary: &ServerName,
	batch: S,
) -> Result<Vec<ServerSigningKeys>>
where
	S: Iterator<Item = (&'a ServerName, K)> + Send,
	K: Iterator<Item = &'a ServerSigningKeyId> + Send,
{
	use get_remote_server_keys_batch::v2::Request;
	type RumaBatch = BTreeMap<OwnedServerName, BTreeMap<OwnedServerSigningKeyId, QueryCriteria>>;

	let criteria = QueryCriteria {
		minimum_valid_until_ts: Some(self.minimum_valid_ts()),
	};

	let mut server_keys = batch.fold(RumaBatch::new(), |mut batch, (server, key_ids)| {
		batch
			.entry(server.into())
			.or_default()
			.extend(key_ids.map(|key_id| (key_id.into(), criteria.clone())));

		batch
	});

	let total_keys = server_keys
		.iter()
		.flat_map(|(_, ids)| ids.iter())
		.count();

	debug_assert!(total_keys > 0, "empty batch request to notary");

	let batch_max = self
		.services
		.server
		.config
		.trusted_server_batch_size;

	let batch_concurrency = self
		.services
		.server
		.config
		.trusted_server_batch_concurrency;

	let batches: Vec<_> = server_keys
		.keys()
		.rev()
		.step_by(batch_max.saturating_sub(1))
		.skip(1)
		.chain(server_keys.keys().next().into_iter())
		.cloned()
		.collect();

	batches
		.iter()
		.stream()
		.enumerate()
		.map(|(i, batch)| {
			let request = Request {
				server_keys: server_keys.split_off(batch),
			};

			if request.server_keys.is_empty() {
				return None;
			}

			trace!(
				%i, %notary, ?batch,
				remaining = ?server_keys,
				requesting = ?request.server_keys.keys(),
				"Request to notary server."
			);

			info!(
				%notary,
				remaining = %server_keys.len(),
				requesting = %request.server_keys.len(),
				"Sending request to notary server..."
			);

			Some(Ok(request))
		})
		.ready_filter_map(identity)
		.broadn_and_then(batch_concurrency, |request| {
			self.services
				.federation
				.execute_synapse(notary, request)
		})
		.ready_try_fold(Vec::new(), |mut results, response| {
			let response = response
				.server_keys
				.into_iter()
				.map(|key| key.deserialize())
				.filter_map(Result::ok);

			trace!(
				%notary, ?response,
				"Response from notary server."
			);

			results.extend(response);

			info!(
				"Received {0} keys out of {1} from notary server so far...",
				results.len(),
				total_keys,
			);

			Ok(results)
		})
		.inspect_err(|e| {
			error!(
				?notary, %batch_max, %batch_concurrency, %total_keys,
				"Requesting keys from notary server failed: {e}",
			);
		})
		.boxed()
		.await
}

#[implement(super::Service)]
pub async fn notary_request(
	&self,
	notary: &ServerName,
	target: &ServerName,
) -> Result<impl Iterator<Item = ServerSigningKeys> + Clone + Debug + Send + use<>> {
	use get_remote_server_keys::v2::Request;

	let request = Request {
		server_name: target.into(),
		minimum_valid_until_ts: self.minimum_valid_ts(),
	};

	let response = self
		.services
		.federation
		.execute(notary, request)
		.await?
		.server_keys
		.into_iter()
		.map(|key| key.deserialize())
		.filter_map(Result::ok);

	Ok(response)
}

#[implement(super::Service)]
pub async fn server_request(&self, target: &ServerName) -> Result<ServerSigningKeys> {
	use get_server_keys::v2::Request;

	let server_signing_key = self
		.services
		.federation
		.execute(target, Request::new())
		.await
		.map(|response| response.server_key)
		.and_then(|key| key.deserialize().map_err(Into::into))?;

	if server_signing_key.server_name != target {
		return Err!(BadServerResponse(debug_warn!(
			requested = ?target,
			response = ?server_signing_key.server_name,
			"Server responded with bogus server_name"
		)));
	}

	Ok(server_signing_key)
}
