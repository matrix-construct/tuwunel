//! Synapse-compatible shared-secret registration backend.
//!
//! Pairs with the HTTP handlers in `tuwunel_api_admin` that serve
//! `/_synapse/admin/v1/register`. Owns:
//!
//! 1. resolution of the shared secret (from `registration_shared_secret` or its
//!    `_file` companion), performed at each use;
//! 2. a short-lived in-memory nonce store with a 60-second TTL.
//!
//! The nonce store lives in RAM rather than RocksDB on purpose: each entry's
//! useful lifespan is shorter than a single block-cache eviction tick, and
//! the working set is bounded by [`NONCE_CAP`] regardless of traffic.

use std::{
	collections::BTreeMap,
	time::{Duration, Instant},
};

use tuwunel_core::{
	implement,
	utils::{self, Secret, is_secret_set, resolve_secret},
};

type Nonces = BTreeMap<String, Instant>;

const NONCE_LENGTH: usize = 32;
const NONCE_TTL: Duration = Duration::from_mins(1);
const NONCE_CAP: usize = 2048;

#[implement(super::Service)]
pub fn issue_register_nonce(&self) -> String {
	let nonce = utils::random_string(NONCE_LENGTH);
	let mut nonces = self
		.register_nonces
		.lock()
		.expect("nonce mutex not poisoned");

	gc_expired(&mut nonces);
	if nonces.len() >= NONCE_CAP {
		drop_oldest(&mut nonces);
	}

	nonces.insert(nonce.clone(), Instant::now());
	nonce
}

/// Consume `nonce` if it exists and has not expired. The entry is removed
/// either way; `true` means the caller may proceed.
#[implement(super::Service)]
pub fn consume_register_nonce(&self, nonce: &str) -> bool {
	let mut nonces = self
		.register_nonces
		.lock()
		.expect("nonce mutex not poisoned");

	nonces
		.remove(nonce)
		.is_some_and(|issued| issued.elapsed() < NONCE_TTL)
}

/// Answered without opening the secret file, since the nonce endpoint this
/// gates takes no authentication and is not rate limited.
#[implement(super::Service)]
pub fn register_is_enabled(&self) -> bool {
	let config = &self.services.server.config;

	is_secret_set(
		config.registration_shared_secret_file.as_deref(),
		config.registration_shared_secret.as_deref(),
	)
}

/// Reads `registration_shared_secret_file` on every call, so a rotated secret
/// takes effect without a restart.
#[implement(super::Service)]
pub fn register_shared_secret(&self) -> Option<Secret> {
	let config = &self.services.server.config;

	resolve_secret(
		config.registration_shared_secret_file.as_deref(),
		config.registration_shared_secret.as_deref(),
		"registration shared secret",
	)
}

fn drop_oldest(nonces: &mut Nonces) {
	nonces
		.iter()
		.min_by_key(|(_, issued)| **issued)
		.map(|(k, _)| k.clone())
		.as_ref()
		.map(|oldest| nonces.remove(oldest));
}

fn gc_expired(nonces: &mut Nonces) { nonces.retain(|_, issued| issued.elapsed() < NONCE_TTL); }
