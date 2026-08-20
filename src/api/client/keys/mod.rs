mod claim_keys;
mod get_key_changes;
mod get_keys;
mod upload_keys;
mod upload_signatures;
mod upload_signing_keys;

use std::{collections::BTreeMap, num::NonZeroUsize, time::Duration};

pub(crate) use claim_keys::{claim_keys_helper, claim_keys_route};
pub(crate) use get_key_changes::get_key_changes_route;
pub(crate) use get_keys::{get_keys_helper, get_keys_route};
use serde_json::{Value as JsonValue, json};
use tuwunel_core::debug_warn;
use tuwunel_service::{
	Services,
	federation::feds::{Fault, Faults, Opts},
};
pub(crate) use upload_keys::upload_keys_route;
pub(crate) use upload_signatures::upload_signatures_route;
pub(crate) use upload_signing_keys::upload_signing_keys_route;

type FailureMap = BTreeMap<String, JsonValue>;

fn federation_opts(services: &Services) -> Opts {
	Opts {
		width: NonZeroUsize::new(services.server.config.feds_max_width),
		timeout: Some(Duration::from_secs(services.server.config.federation_keys_timeout)),
		..Default::default()
	}
}

fn federation_failures(
	endpoint: &'static str,
	faults: Faults,
) -> impl Iterator<Item = (String, JsonValue)> {
	faults.into_iter().map(move |(server, fault)| {
		match &fault {
			| Fault::Error(error) =>
				debug_warn!(%server, %error, endpoint, "key federation request failed"),
			| _ => debug_warn!(%server, ?fault, endpoint, "key federation request failed"),
		}

		(server.to_string(), json!({}))
	})
}
