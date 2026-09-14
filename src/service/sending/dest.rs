use std::fmt::Debug;

use ruma::{OwnedServerName, OwnedUserId};
use tuwunel_core::{implement, matrix::pdu::RawPduId};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Destination {
	Appservice(String),
	Push(OwnedUserId, String), // user and pushkey
	Federation(OwnedServerName),
}

#[implement(Destination)]
#[inline]
#[must_use]
pub(super) fn event_key(&self, pdu_id: &RawPduId) -> Vec<u8> {
	self.suffixed_key(pdu_id.as_ref())
}

#[implement(Destination)]
#[inline]
#[must_use]
pub(super) fn count_key(&self, count: u64) -> Vec<u8> { self.suffixed_key(&count.to_be_bytes()) }

#[implement(Destination)]
#[must_use]
fn suffixed_key(&self, suffix: &[u8]) -> Vec<u8> {
	let mut key = self.get_prefix_with_capacity(suffix.len());

	key.extend_from_slice(suffix);
	key
}

#[implement(Destination)]
#[inline]
#[must_use]
pub(super) fn get_prefix(&self) -> Vec<u8> { self.get_prefix_with_capacity(0) }

#[implement(Destination)]
#[must_use]
pub(super) fn get_prefix_with_capacity(&self, additional: usize) -> Vec<u8> {
	match self {
		| Self::Federation(server) => {
			let len = server
				.as_bytes()
				.len()
				.saturating_add(1)
				.saturating_add(additional);

			let mut p = Vec::with_capacity(len);
			p.extend_from_slice(server.as_bytes());
			p.push(0xFF);
			p
		},
		| Self::Appservice(server) => {
			let sigil = b"+";
			let len = sigil
				.len()
				.saturating_add(server.len())
				.saturating_add(1)
				.saturating_add(additional);

			let mut p = Vec::with_capacity(len);
			p.extend_from_slice(sigil);
			p.extend_from_slice(server.as_bytes());
			p.push(0xFF);
			p
		},
		| Self::Push(user, pushkey) => {
			let sigil = b"$";
			let len = sigil
				.len()
				.saturating_add(user.as_bytes().len())
				.saturating_add(1)
				.saturating_add(pushkey.len())
				.saturating_add(1)
				.saturating_add(additional);

			let mut p = Vec::with_capacity(len);
			p.extend_from_slice(sigil);
			p.extend_from_slice(user.as_bytes());
			p.push(0xFF);
			p.extend_from_slice(pushkey.as_bytes());
			p.push(0xFF);
			p
		},
	}
}
