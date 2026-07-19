// RAM fits 4 KiB, minute-lived sessions; restarts end them like OAuth state.

use std::{
	cmp::max,
	collections::BTreeMap,
	str,
	sync::{Arc, RwLock},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as b64};
use bytes::Bytes;
use tuwunel_core::{
	Result,
	arrayvec::ArrayString,
	implement,
	utils::{hash::sha256::concat, rand::string_array, time::duration_since_epoch},
};

pub type SessionId = ArrayString<SESSION_ID_LENGTH>;
pub type Etag = ArrayString<ETAG_LENGTH>;
type Sessions = BTreeMap<SessionId, Session>;

pub struct Service {
	sessions: RwLock<Sessions>,
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

const SESSION_ID_LENGTH: usize = 32;
const ETAG_VALUE_LENGTH: usize = 43;
const ETAG_LENGTH: usize = ETAG_VALUE_LENGTH + 2;
const MILLIS_PER_SECOND: u64 = 1000;
const MAX_HTTP_DATE_SECONDS: u64 = 253_402_300_799;
const MONOTONIC_STEP: Duration = Duration::from_millis(1);

#[cfg(test)]
mod tests;

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			sessions: RwLock::new(Sessions::new()),
			services: args.services.clone(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
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
	let session = Session {
		etag: etag(&data, now),
		data,
		created: now,
		last_modified: now,
		expires_at,
	};

	let meta = session.meta();

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

	if !etag_matches(if_match, &session.etag) {
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
		etag: etag(&data, last_modified),
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

impl Session {
	fn meta(&self) -> Meta {
		Meta {
			etag: self.etag,
			expires_at: self.expires_at,
			last_modified: self.last_modified,
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

fn etag(data: &Bytes, last_modified: SystemTime) -> Etag {
	let elapsed = duration_since_epoch(last_modified);
	let millis = elapsed
		.as_secs()
		.saturating_mul(MILLIS_PER_SECOND)
		.saturating_add(u64::from(elapsed.subsec_millis()));

	let timestamp = millis.to_be_bytes();
	let digest = concat([data.as_ref(), timestamp.as_slice()].into_iter());
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
