mod claim_keys;
mod get_key_changes;
mod get_keys;
mod upload_keys;
mod upload_signatures;
mod upload_signing_keys;

use std::{collections::BTreeMap, num::NonZeroUsize, time::Duration};

pub use claim_keys::claim_keys_helper;
pub(crate) use claim_keys::claim_keys_route;
pub(crate) use get_key_changes::get_key_changes_route;
pub use get_keys::get_keys_helper;
pub(crate) use get_keys::get_keys_route;
use ruma::serde::Raw;
use serde_json::{Value as JsonValue, json};
use tuwunel_core::{Err, Result, debug_warn};
use tuwunel_service::{
	Services,
	federation::feds::{Fault, Faults, Opts},
};
pub(crate) use upload_keys::upload_keys_route;
pub(crate) use upload_signatures::upload_signatures_route;
pub(crate) use upload_signing_keys::upload_signing_keys_route;

use crate::MAX_EDU_CONTENT_BYTES;

type FailureMap = BTreeMap<String, JsonValue>;

/// Checks one raw key before any part of its upload is applied.
pub(super) fn check_key_size<T>(key: &Raw<T>) -> Result {
	if key.json().get().len() > MAX_EDU_CONTENT_BYTES {
		return Err!(Request(TooLarge("Key object exceeds {MAX_EDU_CONTENT_BYTES} bytes.")));
	}

	Ok(())
}

fn federation_opts(services: &Services) -> Opts {
	let config = &services.server.config.federation;

	Opts {
		width: NonZeroUsize::new(config.fanout.max_width),
		timeout: Some(Duration::from_secs(config.transport.keys_timeout)),
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
