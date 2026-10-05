use std::{net::IpAddr, sync::Arc, time::SystemTime};

use futures::{Stream, StreamExt, future::join};
use ruma::ServerName;
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Result,
	arrayvec::ArrayVec,
	at, err, implement,
	utils::{math::Expected, rand, stream::TryIgnore},
};
use tuwunel_database::{Cbor, Deserialized, Map};

use super::{DestString, FedDest};

pub struct Cache {
	destinations: Arc<Map>,
	overrides: Arc<Map>,
}

/// A server name's discovered federation destination, cached until `expire`.
///
/// The route kind (`srv`) has no serde default on purpose: rows written before
/// it fail to decode, so their destination is rediscovered rather than guessed.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CachedDest {
	/// Authority federation requests are addressed to, with its port.
	pub dest: FedDest,

	/// The server name or its well-known delegate, with port.
	pub host: DestString,

	/// When the entry lapses and the destination is discovered again.
	pub expire: SystemTime,

	/// Whether destination discovery selected an SRV route.
	pub srv: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CachedOverride {
	pub ips: IpAddrs,
	pub port: u16,
	pub expire: SystemTime,
	pub overriding: Option<DestString>,
}

pub type IpAddrs = ArrayVec<IpAddr, MAX_IPS>;
pub(crate) const MAX_IPS: usize = 3;

impl Cache {
	pub(super) fn new(args: &crate::Args<'_>) -> Arc<Self> {
		Arc::new(Self {
			destinations: args.db["servername_destination"].clone(),
			overrides: args.db["servername_override"].clone(),
		})
	}
}

#[implement(Cache)]
pub async fn clear(&self) { join(self.clear_destinations(), self.clear_overrides()).await; }

#[implement(Cache)]
pub async fn clear_destinations(&self) { self.destinations.clear().await; }

#[implement(Cache)]
pub async fn clear_overrides(&self) { self.overrides.clear().await; }

#[implement(Cache)]
pub fn del_destination(&self, name: &ServerName) { self.destinations.remove(name); }

#[implement(Cache)]
pub fn del_override(&self, name: &str) { self.overrides.remove(name); }

#[implement(Cache)]
pub fn set_destination(&self, name: &ServerName, dest: &CachedDest) {
	self.destinations.raw_put(name, Cbor(dest));
}

#[implement(Cache)]
pub fn set_override(&self, name: &str, over: &CachedOverride) {
	self.overrides.raw_put(name, Cbor(over));
}

#[implement(Cache)]
#[must_use]
pub async fn has_destination(&self, destination: &ServerName) -> bool {
	self.get_destination(destination).await.is_ok()
}

/// Whether a valid override for `name` already points at `hostname`.
///
/// Only SRV routes write overrides, so a row that does not match is replaced
/// rather than reused.
#[implement(Cache)]
#[must_use]
pub async fn has_override(&self, name: &str, hostname: &str) -> bool {
	self.get_override(name)
		.await
		.is_ok_and(|cached| cached.covers(hostname))
}

/// Whether this override is valid and points at `hostname`.
///
/// A plain row (no `overriding`) is left by an older release and never matches.
#[implement(CachedOverride)]
#[inline]
#[must_use]
pub fn covers(&self, hostname: &str) -> bool {
	self.valid() && self.overriding.as_deref() == Some(hostname)
}

#[implement(Cache)]
pub async fn get_destination(&self, name: &ServerName) -> Result<CachedDest> {
	self.destinations
		.get(name)
		.await
		.deserialized::<Cbor<_>>()
		.map(at!(0))
		.into_iter()
		.find(CachedDest::valid)
		.ok_or(err!(Request(NotFound("Expired from cache"))))
}

#[implement(Cache)]
pub async fn get_override(&self, name: &str) -> Result<CachedOverride> {
	self.overrides
		.get(name)
		.await
		.deserialized::<Cbor<_>>()
		.map(at!(0))
}

#[implement(Cache)]
pub fn destinations(&self) -> impl Stream<Item = (&ServerName, CachedDest)> + Send + '_ {
	self.destinations
		.stream()
		.ignore_err()
		.map(|item: (&ServerName, Cbor<_>)| (item.0, item.1.0))
}

#[implement(Cache)]
pub fn overrides(&self) -> impl Stream<Item = (&ServerName, CachedOverride)> + Send + '_ {
	self.overrides
		.stream()
		.ignore_err()
		.map(|item: (&ServerName, Cbor<_>)| (item.0, item.1.0))
}

impl CachedDest {
	#[inline]
	#[must_use]
	pub fn valid(&self) -> bool { self.expire > SystemTime::now() }

	#[must_use]
	pub(crate) fn default_expire() -> SystemTime {
		rand::time_from_now_secs(60 * 60 * 18..60 * 60 * 36)
	}

	#[inline]
	#[must_use]
	pub fn size(&self) -> usize {
		self.dest
			.size()
			.expected_add(self.host.len())
			.expected_add(size_of_val(&self.expire))
			.expected_add(size_of_val(&self.srv))
	}
}

impl CachedOverride {
	#[inline]
	#[must_use]
	pub fn valid(&self) -> bool { self.expire > SystemTime::now() }

	#[must_use]
	pub(crate) fn default_expire() -> SystemTime {
		rand::time_from_now_secs(60 * 60 * 6..60 * 60 * 12)
	}

	#[inline]
	#[must_use]
	pub fn size(&self) -> usize { size_of_val(self) }
}
