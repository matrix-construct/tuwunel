//! Email third-party identifier bindings and verification sessions.
//!
//! The service stores bidirectional email bindings, durable pending proofs,
//! and UIAA ownership claims. Process-local token buckets separately limit
//! verification requests by caller address and canonical email.

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

use ruma::{MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, thirdparty::Medium};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Result, smallstr::SmallString, utils::MutexMap};
use tuwunel_database::{Database, Map};

/// Public helpers and result types exposed by the threepid service.
///
/// Email canonicalization produces storage keys, while pending outcomes tell
/// callers whether a verification message must be sent.
/// Both APIs preserve the service's canonical addressing rules.
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
	pending_mutex: MutexMap<String, ()>,
	claim_mutex: MutexMap<UiaaKey, ()>,
	ip_ratelimiter: Ratelimiter<IpAddr>,
	address_ratelimiter: Ratelimiter<EmailKey>,
}

struct Data {
	database: Arc<Database>,
	userid_email: Arc<Map>,
	email_userid: Arc<Map>,
	threepidsid_pending: Arc<Map>,
	userdevicesessionid_threepid: Arc<Map>,
}

/// Stores a UIAA session identifier inline in the common case.
///
/// The 32-byte budget matches identifiers minted by the UIAA service.
/// Longer identifiers spill to the backing allocation without changing their
/// value semantics.
pub type UiaaSessionId = SmallString<[u8; 32]>;

/// Identifies the exact UIAA session that owns a validated threepid.
///
/// Owned components let the durable claim key outlive an individual request.
/// The tuple scopes a claim by user, device, and UIAA session identifier.
pub type UiaaKey = (OwnedUserId, OwnedDeviceId, UiaaSessionId);

/// CBOR value of a `userid_email` row: the per-binding metadata, with the
/// address carried in the composite key.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Binding {
	medium: Medium,
	validated_at: MilliSecondsSinceUnixEpoch,
	added_at: MilliSecondsSinceUnixEpoch,
}

/// Validated third-party identifier consumed from a pending proof.
///
/// Redemption returns the original medium and address stored with the
/// verification session. Owning both values lets the result outlive the
/// pending-row read.
#[derive(Clone, Debug)]
pub struct Association {
	/// Third-party identifier medium that was verified.
	pub medium: Medium,

	/// Address exactly as stored by the verification session.
	pub address: String,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				database: args.db.clone(),
				userid_email: args.db["userid_email"].clone(),
				email_userid: args.db["email_userid"].clone(),
				threepidsid_pending: args.db["threepidsid_pending"].clone(),
				userdevicesessionid_threepid: args.db["userdevicesessionid_threepid"].clone(),
			},
			pending_mutex: MutexMap::new(),
			claim_mutex: MutexMap::new(),
			ip_ratelimiter: Mutex::new(HashMap::new()),
			address_ratelimiter: Mutex::new(HashMap::new()),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}
