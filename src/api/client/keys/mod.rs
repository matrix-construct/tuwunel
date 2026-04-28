mod claim_keys;
mod get_key_changes;
mod get_keys;
mod upload_keys;
mod upload_signatures;
mod upload_signing_keys;

use std::collections::BTreeMap;

pub(crate) use claim_keys::{claim_keys_helper, claim_keys_route};
pub(crate) use get_key_changes::get_key_changes_route;
pub(crate) use get_keys::{get_keys_helper, get_keys_route};
use serde_json::Value as JsonValue;
pub(crate) use upload_keys::upload_keys_route;
pub(crate) use upload_signatures::upload_signatures_route;
pub(crate) use upload_signing_keys::upload_signing_keys_route;

type FailureMap = BTreeMap<String, JsonValue>;
