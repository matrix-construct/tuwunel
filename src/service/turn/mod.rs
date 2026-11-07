//! Fetches TURN credentials from an external provider.
//!
//! One set of credentials is shared by every user and renewed after half its
//! lifetime, so the provider sees a bounded number of requests however many
//! clients ask. A failed fetch pauses further attempts for a minute, while any
//! unexpired credentials keep being served.

mod provider;
#[cfg(test)]
mod tests;

use std::{
	module_path,
	sync::Arc,
	time::{Duration, Instant},
};

use async_trait::async_trait;
use ruma::api::client::voip::get_turn_server_info::v3::Response as Credentials;
use tokio::sync::Mutex;
use tuwunel_core::{Error, Result, config::turn::Provider, err, implement, warn};

use crate::{Args, SelfServices, service::make_name};

/// Shares one set of externally issued TURN credentials among all users.
///
/// Its lock also makes concurrent callers wait for one fetch.
pub struct Service {
	services: SelfServices,

	/// One entry, replaced after half the provider's lifetime, and kept out of
	/// the database because it holds a secret.
	cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
	origin: String,
	entry: Option<Entry>,
	failed: Option<Instant>,
}

struct Entry {
	credentials: Credentials,
	renews: Instant,
	expires: Instant,
}

/// What the cache offers a caller.
///
/// Only a fetch that is due goes to the provider; the other cases are answered
/// from the cache.
enum Lookup {
	/// Credentials young enough to serve without renewal.
	Fresh(Credentials),

	/// A fetch is due.
	Due,

	/// A recent failure holds fetches back; whatever is still unexpired is served.
	Held(Option<Credentials>),
}

/// Pause after a failed fetch before the provider is asked again.
///
/// Some clients retry every few seconds while the endpoint fails, and the pause
/// keeps them from turning a provider outage into a stream of requests to it.
const RETRY_DELAY: Duration = Duration::from_mins(1);

#[async_trait]
impl crate::Service for Service {
	fn build(args: &Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			cache: Mutex::default(),
		}))
	}

	async fn clear_cache(&self) { *self.cache.lock().await = Cache::default(); }

	fn name(&self) -> &str { make_name(module_path!()) }
}

/// Returns TURN credentials from the configured provider.
///
/// Cached credentials are served until half their lifetime has passed, then
/// renewed. When the renewal fails, what is still unexpired at that moment is
/// served, or else 502; with no provider configured the answer is 404.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn credentials(&self) -> Result<Credentials> {
	// A snapshot, because the provider settings stay borrowed across the fetch.
	let config = Arc::clone(&self.services.server.config);
	let provider = config
		.turn
		.provider()
		.ok_or_else(|| err!(Request(NotFound("No TURN credential provider is configured."))))?;

	let origin = origin(provider);
	let mut cache = self.cache.lock().await;
	let now = Instant::now();
	match cache.lookup(origin, now) {
		| Lookup::Fresh(credentials) => return Ok(credentials),
		| Lookup::Held(remaining) => return remaining.ok_or_else(unavailable),
		| Lookup::Due => {},
	}

	match self.fetch(&config.turn, provider).await {
		| Ok(credentials) => {
			*cache = Cache::fetched(origin, credentials.clone(), now);
			Ok(credentials)
		},
		| Err(error) => {
			warn!(%error, "Failed to fetch TURN credentials");
			let failed = Instant::now();
			cache.record_failure(origin, failed);
			cache.remaining(failed).ok_or_else(unavailable)
		},
	}
}

#[implement(Cache)]
fn lookup(&self, origin: &str, now: Instant) -> Lookup {
	if self.origin != origin {
		return Lookup::Due;
	}

	let fresh = self
		.entry
		.as_ref()
		.is_some_and(|entry| now < entry.renews);

	let held = self.failed.is_some_and(|failed| {
		now.saturating_duration_since(failed)
			.lt(&RETRY_DELAY)
	});

	match (fresh, held) {
		| (true, _) => self
			.remaining(now)
			.map_or(Lookup::Due, Lookup::Fresh),
		| (false, true) => Lookup::Held(self.remaining(now)),
		| (false, false) => Lookup::Due,
	}
}

#[implement(Cache)]
fn remaining(&self, now: Instant) -> Option<Credentials> {
	self.entry
		.as_ref()
		.and_then(|entry| entry.remaining(now))
}

impl Entry {
	/// The cached credentials with their lifetime cut to what is left of it.
	///
	/// Expired credentials yield `None`.
	fn remaining(&self, now: Instant) -> Option<Credentials> {
		self.expires
			.checked_duration_since(now)
			.filter(|ttl| !ttl.is_zero())
			.map(|ttl| Credentials { ttl, ..self.credentials.clone() })
	}
}

#[implement(Cache)]
fn fetched(origin: &str, credentials: Credentials, now: Instant) -> Self {
	let half = credentials.ttl.checked_div(2).unwrap_or_default();
	let entry = Entry {
		renews: now.checked_add(half).unwrap_or(now),
		expires: now.checked_add(credentials.ttl).unwrap_or(now),
		credentials,
	};

	Self {
		origin: origin.to_owned(),
		entry: Some(entry),
		failed: None,
	}
}

/// Records a failed fetch from `origin`.
///
/// What another provider left in the cache is discarded, so a failure never
/// holds back a provider it did not come from.
#[implement(Cache)]
fn record_failure(&mut self, origin: &str, at: Instant) {
	let entry = self
		.entry
		.take()
		.filter(|_| self.origin == origin);

	*self = Self {
		origin: origin.to_owned(),
		entry,
		failed: Some(at),
	};
}

/// Identifies the provider cached credentials came from.
///
/// A reload that names another provider then finds nothing cached for it.
fn origin(provider: Provider<'_>) -> &str {
	match provider {
		| Provider::Endpoint(_, url) => url.as_str(),
		| Provider::Cloudflare(_, key_id) => key_id,
	}
}

fn unavailable() -> Error { err!(Request(ConnectionFailed("TURN credentials are unavailable."))) }
