//! In-memory rendezvous session exchange.
//!
//! The service holds short-lived client rendezvous payloads, conditional validators, and per-client
//! rate-limit buckets. Sessions are bounded in count and intentionally disappear on restart.

use std::{
	cmp::max,
	collections::{BTreeMap, HashMap},
	net::IpAddr,
	str,
	sync::{Arc, Mutex, RwLock},
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use bytes::Bytes;
use http::StatusCode;
use ruma::api::error::{ErrorKind, LimitExceededErrorData};
use tuwunel_core::{
	Error, Result,
	arrayvec::ArrayString,
	implement,
	utils::{hash::sha256::concat, rand::string_array, time::duration_since_epoch},
};

/// Fixed-capacity identifier for a rendezvous session.
///
/// New sessions receive a random 32-character ASCII identifier.
pub type SessionId = ArrayString<SESSION_ID_LENGTH>;

/// Quoted HTTP entity tag for rendezvous payload state.
///
/// The fixed capacity holds the encoded digest and its surrounding quotation marks.
pub type Etag = ArrayString<ETAG_LENGTH>;
type Sessions = BTreeMap<SessionId, Session>;
type Ratelimiter = Mutex<HashMap<IpAddr, (Instant, f64)>>;

/// Stores bounded rendezvous sessions and request rate-limit state.
///
/// Both stores are process-local and shared through locks. Expired sessions are removed lazily
/// during creation, retrieval, or conditional mutation.
pub struct Service {
	sessions: RwLock<Sessions>,
	// At most 4096 short-lived per-IP buckets.
	ratelimiter: Ratelimiter,
	services: Arc<crate::services::OnceServices>,
}

/// Complete process-local state for one rendezvous session.
///
/// Creation time determines capacity eviction, while modification and expiration times drive HTTP
/// metadata and lifecycle decisions.
struct Session {
	data: Bytes,
	etag: Etag,
	created: SystemTime,
	last_modified: SystemTime,
	expires_at: SystemTime,
}

/// HTTP metadata associated with a rendezvous payload.
///
/// The entity tag and modification time identify the current representation. Expiration determines
/// how long the session remains active.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Meta {
	/// Quoted entity tag for conditional requests.
	pub etag: Etag,

	/// Absolute time at which the session expires.
	pub expires_at: SystemTime,

	/// Last payload modification time.
	pub last_modified: SystemTime,
}

/// Outcome of retrieving a rendezvous session.
///
/// A matching conditional validator returns metadata without cloning the payload. Missing and
/// expired sessions share the not-found outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Get {
	/// Current payload and its HTTP metadata.
	Data {
		/// Opaque rendezvous payload.
		data: Bytes,

		/// Metadata describing the returned representation.
		meta: Meta,
	},

	/// Metadata for a representation matching the request validator.
	NotModified(Meta),

	/// Session was absent or expired.
	NotFound,
}

/// Outcome of conditionally replacing a rendezvous payload.
///
/// Accepted updates return current metadata, including idempotent retries. A stale validator for
/// different data returns the existing metadata as a precondition failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Put {
	/// Replacement or idempotent retry was accepted.
	Accepted(Meta),

	/// Validator was stale and the submitted payload differed.
	PreconditionFailed(Meta),

	/// Session was absent or expired.
	NotFound,
}

/// Conditional validator accepted by the update state machine.
///
/// HTTP updates use the quoted entity tag, while protocol sequence-token updates use its unquoted
/// value.
#[derive(Clone, Copy)]
enum Validator<'a> {
	/// Quoted HTTP entity tag supplied through `If-Match`.
	Etag(&'a str),

	/// Unquoted protocol sequence token.
	SequenceToken(&'a str),
}

const SESSION_ID_LENGTH: usize = 32;
const ETAG_VALUE_LENGTH: usize = 43;
const ETAG_LENGTH: usize = ETAG_VALUE_LENGTH + 2;
const MILLIS_PER_SECOND: u64 = 1000;
const MAX_HTTP_DATE_SECONDS: u64 = 253_402_300_799;
const MONOTONIC_STEP: Duration = Duration::from_millis(1);
const RATELIMIT_MAP_CAP: usize = 4096;

#[cfg(test)]
mod tests;

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			sessions: RwLock::new(Sessions::new()),
			ratelimiter: Mutex::new(HashMap::new()),
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Charges one request against a client's rendezvous rate-limit bucket.
///
/// Buckets refill at the configured rate up to the configured burst size. A depleted bucket returns
/// a limit-exceeded request error, and the per-client table remains bounded.
#[implement(Service)]
pub fn check_rate_limit(&self, client: IpAddr) -> Result {
	let config = &self.services.server.config;
	let rate = f64::from(config.rendezvous_rc_per_second.max(1));
	let burst = f64::from(config.rendezvous_rc_burst_count.max(1));

	check_bucket_at(&self.ratelimiter, client, rate, burst, Instant::now())
}

fn check_bucket_at(
	table: &Ratelimiter,
	client: IpAddr,
	rate: f64,
	burst: f64,
	now: Instant,
) -> Result {
	let mut buckets = table.lock()?;

	if buckets.len() >= RATELIMIT_MAP_CAP && !buckets.contains_key(&client) {
		let mut oldest = None;

		buckets.retain(|client, (last, tokens)| {
			let refilled = now
				.duration_since(*last)
				.as_secs_f64()
				.mul_add(rate, *tokens);
			let retain = refilled < burst;

			if retain && oldest.is_none_or(|(_, oldest_at)| *last < oldest_at) {
				oldest = Some((*client, *last));
			}

			retain
		});

		if buckets.len() >= RATELIMIT_MAP_CAP
			&& let Some((oldest, _)) = oldest
		{
			buckets.remove(&oldest);
		}
	}

	let (last_time, tokens) = buckets.entry(client).or_insert((now, burst));
	let new_tokens = now
		.duration_since(*last_time)
		.as_secs_f64()
		.mul_add(rate, *tokens)
		.min(burst);

	if new_tokens < 1.0 {
		return Err(Error::Request(
			ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after: None }),
			"Too many rendezvous requests.".into(),
			StatusCode::TOO_MANY_REQUESTS,
		));
	}

	*last_time = now;
	*tokens = new_tokens - 1.0;

	Ok(())
}

/// Creates a rendezvous session for an opaque payload.
///
/// Expired sessions are pruned first, then the oldest live sessions are evicted to honor the
/// configured capacity, with an effective minimum of one. The returned metadata describes the
/// newly stored representation.
///
/// # Panics
///
/// Panics if the session lock has been poisoned.
#[implement(Service)]
pub fn create(&self, data: Bytes) -> (SessionId, Meta) {
	let config = &self.services.server.config;
	let ttl = Duration::from_secs(config.rendezvous_session_ttl);

	self.create_at(data, SystemTime::now(), ttl, config.rendezvous_max_sessions)
}

#[implement(Service)]
fn create_at(
	&self,
	data: Bytes,
	now: SystemTime,
	ttl: Duration,
	max_sessions: usize,
) -> (SessionId, Meta) {
	let capacity = max_sessions.max(1);
	let mut id = string_array::<SESSION_ID_LENGTH>();
	let expires_at = expires_at(now, ttl);

	let mut sessions = self.sessions.write().expect("locked for writing");

	sessions.retain(|_, session| session.expires_at > now);
	while sessions.contains_key(id.as_str()) {
		id = string_array::<SESSION_ID_LENGTH>();
	}

	while sessions.len() >= capacity {
		let Some(id) = sessions
			.iter()
			.min_by_key(|(_, session)| session.created)
			.map(|(id, _)| *id)
		else {
			break;
		};

		sessions.remove(&id);
	}

	let session = Session {
		etag: etag(id.as_str(), &data, now),
		data,
		created: now,
		last_modified: now,
		expires_at,
	};

	let meta = session.meta();

	sessions.insert(id, session);

	(id, meta)
}

/// Retrieves a rendezvous payload with optional entity-tag validation.
///
/// A matching tag or wildcard returns [`Get::NotModified`]; otherwise a live session returns its
/// data. Expired sessions are removed and reported as not found.
///
/// # Panics
///
/// Panics if the session lock has been poisoned.
#[implement(Service)]
pub fn get(&self, id: &str, if_none_match: Option<&str>) -> Get {
	self.get_at(id, if_none_match, SystemTime::now())
}

#[implement(Service)]
fn get_at(&self, id: &str, if_none_match: Option<&str>, now: SystemTime) -> Get {
	{
		let sessions = self.sessions.read().expect("locked for reading");

		match sessions.get(id) {
			| Some(session) if session.expires_at > now => {
				return get_outcome(session, if_none_match);
			},
			| Some(_) => {},
			| None => return Get::NotFound,
		}
	}

	let mut sessions = self.sessions.write().expect("locked for writing");
	if sessions
		.get(id)
		.is_some_and(|session| session.expires_at <= now)
	{
		sessions.remove(id);

		return Get::NotFound;
	}

	sessions
		.get(id)
		.map_or(Get::NotFound, |session| get_outcome(session, if_none_match))
}

/// Conditionally replaces a rendezvous payload using a quoted entity tag.
///
/// A matching validator stores the new payload and advances its metadata. A stale validator accepts
/// identical data as a retry but rejects different data with the current metadata.
///
/// # Panics
///
/// Panics if the session lock has been poisoned.
#[implement(Service)]
pub fn put(&self, id: &str, if_match: &str, data: Bytes) -> Put {
	let ttl = Duration::from_secs(self.services.server.config.rendezvous_session_ttl);

	self.put_at(id, if_match, data, SystemTime::now(), ttl)
}

#[implement(Service)]
fn put_at(&self, id: &str, if_match: &str, data: Bytes, now: SystemTime, ttl: Duration) -> Put {
	self.put_with_at(id, Validator::Etag(if_match), data, now, ttl)
}

/// Conditionally replaces a rendezvous payload using an unquoted sequence token.
///
/// Update and retry behavior matches [`Self::put`], but the validator is compared with the entity
/// tag's unquoted value.
///
/// # Panics
///
/// Panics if the session lock has been poisoned.
#[implement(Service)]
pub fn put_token(&self, id: &str, sequence_token: &str, data: Bytes) -> Put {
	let ttl = Duration::from_secs(self.services.server.config.rendezvous_session_ttl);

	self.put_token_at(id, sequence_token, data, SystemTime::now(), ttl)
}

#[implement(Service)]
fn put_token_at(
	&self,
	id: &str,
	sequence_token: &str,
	data: Bytes,
	now: SystemTime,
	ttl: Duration,
) -> Put {
	self.put_with_at(id, Validator::SequenceToken(sequence_token), data, now, ttl)
}

/// Applies the shared conditional-update state machine at a supplied time.
///
/// Expired sessions are removed before validation. Successful replacements advance modification
/// time monotonically, while identical stale retries only refresh expiration.
#[implement(Service)]
fn put_with_at(
	&self,
	id: &str,
	validator: Validator<'_>,
	data: Bytes,
	now: SystemTime,
	ttl: Duration,
) -> Put {
	let mut sessions = self.sessions.write().expect("locked for writing");

	if sessions
		.get(id)
		.is_some_and(|session| session.expires_at <= now)
	{
		sessions.remove(id);

		return Put::NotFound;
	}

	let Some(session) = sessions.get_mut(id) else {
		return Put::NotFound;
	};

	if !validator.matches(&session.etag) {
		if data != session.data {
			return Put::PreconditionFailed(session.meta());
		}

		session.expires_at = expires_at(now, ttl);

		return Put::Accepted(session.meta());
	}

	let created = session.created;
	let last_modified = next_last_modified(now, session.last_modified);
	let expires_at = expires_at(now, ttl);

	*session = Session {
		etag: etag(id, &data, last_modified),
		data,
		created,
		last_modified,
		expires_at,
	};

	Put::Accepted(session.meta())
}

/// Deletes a rendezvous session regardless of expiration.
///
/// The return value reports whether any stored session was removed, including an expired one.
///
/// # Panics
///
/// Panics if the session lock has been poisoned.
#[implement(Service)]
pub fn delete(&self, id: &str) -> bool {
	self.sessions
		.write()
		.expect("locked for writing")
		.remove(id)
		.is_some()
}

/// Deletes a rendezvous session and reports whether it was active.
///
/// Expired sessions are still removed but return `false`. An absent session also returns `false`.
///
/// # Panics
///
/// Panics if the session lock has been poisoned.
#[implement(Service)]
pub fn delete_if_active(&self, id: &str) -> bool {
	self.delete_if_active_at(id, SystemTime::now())
}

#[implement(Service)]
fn delete_if_active_at(&self, id: &str, now: SystemTime) -> bool {
	self.sessions
		.write()
		.expect("locked for writing")
		.remove(id)
		.is_some_and(|session| session.expires_at > now)
}

/// Returns the unquoted sequence token for this representation.
///
/// The token is the digest value contained by the HTTP entity tag.
///
/// # Panics
///
/// Panics if [`Self::etag`] does not contain matching quotation marks.
#[implement(Meta)]
#[must_use]
#[inline]
pub fn sequence_token(&self) -> &str { etag_value(&self.etag) }

fn etag_value(etag: &Etag) -> &str {
	etag.as_str()
		.strip_prefix('"')
		.and_then(|value| value.strip_suffix('"'))
		.expect("ETag is quoted")
}

/// Returns the remaining lifetime of the session.
///
/// Expired sessions report [`Duration::ZERO`] rather than a negative duration.
#[implement(Meta)]
#[must_use]
#[inline]
pub fn expires_in(&self) -> Duration { self.expires_in_at(SystemTime::now()) }

#[implement(Meta)]
fn expires_in_at(&self, now: SystemTime) -> Duration {
	self.expires_at
		.duration_since(now)
		.unwrap_or_default()
}

impl Session {
	fn meta(&self) -> Meta {
		Meta {
			etag: self.etag,
			expires_at: self.expires_at,
			last_modified: self.last_modified,
		}
	}
}

impl Validator<'_> {
	fn matches(self, stored: &Etag) -> bool {
		match self {
			| Self::Etag(candidate) => etag_matches(candidate, stored),
			| Self::SequenceToken(candidate) => candidate == etag_value(stored),
		}
	}
}

fn expires_at(now: SystemTime, ttl: Duration) -> SystemTime {
	let latest = UNIX_EPOCH
		.checked_add(Duration::from_secs(MAX_HTTP_DATE_SECONDS))
		.expect("latest HTTP date should fit in SystemTime");

	now.checked_add(ttl)
		.map_or(latest, |expires_at| expires_at.min(latest))
}

fn next_last_modified(now: SystemTime, previous: SystemTime) -> SystemTime {
	previous
		.checked_add(MONOTONIC_STEP)
		.map_or(now, |next| max(now, next))
}

fn etag(id: &str, data: &Bytes, last_modified: SystemTime) -> Etag {
	let elapsed = duration_since_epoch(last_modified);
	let millis = elapsed
		.as_secs()
		.saturating_mul(MILLIS_PER_SECOND)
		.saturating_add(u64::from(elapsed.subsec_millis()));

	let timestamp = millis.to_be_bytes();
	let digest = concat([id.as_bytes(), data.as_ref(), timestamp.as_slice()].into_iter());
	let mut encoded = [0_u8; ETAG_VALUE_LENGTH];
	let len = b64
		.encode_slice(digest, &mut encoded)
		.expect("ETag buffer has exact capacity");

	let encoded = str::from_utf8(&encoded[..len]).expect("base64url is valid UTF-8");

	Etag::try_from(format_args!("\"{encoded}\"")).expect("ETag has exact capacity")
}

fn get_outcome(session: &Session, if_none_match: Option<&str>) -> Get {
	let meta = session.meta();

	if if_none_match.is_some_and(|candidate| etag_matches(candidate, &session.etag)) {
		Get::NotModified(meta)
	} else {
		Get::Data { data: session.data.clone(), meta }
	}
}

fn etag_matches(candidate: &str, etag: &Etag) -> bool {
	candidate == "*" || candidate == etag.as_str()
}
