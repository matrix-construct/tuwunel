// RAM fits 4 KiB, minute-lived sessions; restarts end them like OAuth state.

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

pub type SessionId = ArrayString<SESSION_ID_LENGTH>;
pub type Etag = ArrayString<ETAG_LENGTH>;
type Sessions = BTreeMap<SessionId, Session>;
type Ratelimiter = Mutex<HashMap<IpAddr, (Instant, f64)>>;

pub struct Service {
	sessions: RwLock<Sessions>,
	// At most 4096 short-lived per-IP buckets.
	ratelimiter: Ratelimiter,
	services: Arc<crate::services::OnceServices>,
}

struct Session {
	data: Bytes,
	etag: Etag,
	created: SystemTime,
	last_modified: SystemTime,
	expires_at: SystemTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Meta {
	pub etag: Etag,
	pub expires_at: SystemTime,
	pub last_modified: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Get {
	Data {
		data: Bytes,
		meta: Meta,
	},
	NotModified(Meta),
	NotFound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Put {
	Accepted(Meta),
	PreconditionFailed(Meta),
	NotFound,
}

#[derive(Clone, Copy)]
enum Validator<'a> {
	Etag(&'a str),
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

#[implement(Service)]
pub fn put(&self, id: &str, if_match: &str, data: Bytes) -> Put {
	let ttl = Duration::from_secs(self.services.server.config.rendezvous_session_ttl);

	self.put_at(id, if_match, data, SystemTime::now(), ttl)
}

#[implement(Service)]
fn put_at(&self, id: &str, if_match: &str, data: Bytes, now: SystemTime, ttl: Duration) -> Put {
	self.put_with_at(id, Validator::Etag(if_match), data, now, ttl)
}

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

#[implement(Service)]
pub fn delete(&self, id: &str) -> bool {
	self.sessions
		.write()
		.expect("locked for writing")
		.remove(id)
		.is_some()
}

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
