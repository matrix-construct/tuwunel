mod binding;
mod canonical;
mod pending;
mod ratelimit;

use std::{
	collections::HashMap,
	net::IpAddr,
	sync::{Arc, Mutex},
	time::Instant,
};

use ruma::{MilliSecondsSinceUnixEpoch, thirdparty::Medium};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, smallstr::SmallString};
use tuwunel_database::Map;

pub use self::{canonical::canonicalize_email, pending::PendingOutcome};

/// Token-bucket table keyed on a throttle axis: last-refill instant and
/// remaining tokens per key.
type Ratelimiter<K> = Mutex<HashMap<K, (Instant, f64)>>;

/// Stack-string key for the per-address throttle bucket; the modal email
/// canonical address fits inline.
type EmailKey = SmallString<[u8; 48]>;

/// Manages email threepid bindings, verification sessions, and request limits.
///
/// Persistent maps provide lookups from user to email and email to user.
/// In-memory token buckets limit `requestToken` calls by caller IP and
/// canonical address.
pub struct Service {
	db: Data,
	ip_ratelimiter: Ratelimiter<IpAddr>,
	address_ratelimiter: Ratelimiter<EmailKey>,
}

struct Data {
	userid_email: Arc<Map>,
	email_userid: Arc<Map>,
	threepidsid_pending: Arc<Map>,
}

/// CBOR value of a `userid_email` row: the per-binding metadata, with the
/// address carried in the composite key.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Binding {
	medium: Medium,
	validated_at: MilliSecondsSinceUnixEpoch,
	added_at: MilliSecondsSinceUnixEpoch,
}

/// Validated `(medium, address)` pair handed back when a pending verification
/// is consumed by the add flow.
#[derive(Clone, Debug)]
pub struct Association {
	pub medium: Medium,
	pub address: String,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				userid_email: args.db["userid_email"].clone(),
				email_userid: args.db["email_userid"].clone(),
				threepidsid_pending: args.db["threepidsid_pending"].clone(),
			},
			ip_ratelimiter: Mutex::new(HashMap::new()),
			address_ratelimiter: Mutex::new(HashMap::new()),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
