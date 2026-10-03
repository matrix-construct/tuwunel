use std::{collections::BTreeSet, iter::repeat};

use futures::TryStreamExt;
use tuwunel_core::{Err, Result, err, implement, utils::TryReadyExt};

use super::{
	identity::{Family, Identities, Kind},
	short_of,
};
use crate::Services;

pub(super) struct References {
	pub(super) events: BTreeSet<u64>,
	pub(super) statekeys: BTreeSet<u64>,
	pub(super) event_complete: bool,
	pub(super) statekey_complete: bool,
}

#[implement(References)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn census(services: &Services, identities: &Identities) -> Result<Self> {
	let db = &services.db;
	let progress = &services.server.progress;
	let aliases = |family: &Family| {
		family
			.candidates
			.iter()
			.filter(|candidate| matches!(candidate.kind, Kind::Alias(_)))
			.map(|candidate| candidate.short)
			.collect::<BTreeSet<_>>()
	};

	let event_aliases = aliases(&identities.events);
	let statekey_aliases = aliases(&identities.statekeys);
	let references = Self {
		events: BTreeSet::new(),
		statekeys: BTreeSet::new(),
		event_complete: true,
		statekey_complete: true,
	};

	if event_aliases.is_empty() && statekey_aliases.is_empty() {
		return Ok(references);
	}

	let references = db["shortstatehash_statediff"]
		.raw_stream()
		.inspect_ok(|_| progress.advance())
		.ready_try_fold(references, |mut references, (_, value)| {
			references.retain_statediff(&event_aliases, &statekey_aliases, value);

			Ok(references)
		})
		.await?;

	if event_aliases.is_empty() {
		return Ok(references);
	}

	let references = db["shorteventid_shortstatehash"]
		.raw_keys()
		.inspect_ok(|_| progress.advance())
		.ready_try_fold(references, |mut references, key| {
			references.retain_event(&event_aliases, key);

			Ok(references)
		})
		.await?;

	let references = db["relatesto_typed"]
		.raw_stream()
		.inspect_ok(|_| progress.advance())
		.ready_try_fold(references, |mut references, (_, value)| {
			references.retain_event(&event_aliases, value);

			Ok(references)
		})
		.await?;

	db["authchainkey_authchain"]
		.raw_stream()
		.inspect_ok(|_| progress.advance())
		.ready_try_fold(references, |mut references, (key, value)| {
			references.event_complete &=
				!key.is_empty() && key.len().is_multiple_of(8) && value.len().is_multiple_of(8);

			key.as_chunks()
				.0
				.iter()
				.chain(value.as_chunks().0.iter())
				.for_each(|bytes: &[u8; 8]| references.retain_event(&event_aliases, bytes));

			Ok(references)
		})
		.await
}

#[implement(References)]
fn retain_statediff(
	&mut self,
	event_aliases: &BTreeSet<u64>,
	statekey_aliases: &BTreeSet<u64>,
	value: &[u8],
) {
	let valid = entries(value)
		.try_for_each(|entry| {
			entry.map(|(statekey, event)| {
				retain(&mut self.statekeys, statekey_aliases, statekey);
				retain(&mut self.events, event_aliases, event);
			})
		})
		.is_ok();

	self.event_complete &= valid;
	self.statekey_complete &= valid;
}

pub(super) fn entries(value: &[u8]) -> impl Iterator<Item = Result<(u64, u64)>> + '_ {
	repeat(()).scan((value.get(8..), false), |state, ()| {
		let (Some(tail), removed) = state else {
			*state = (Some(&[]), false);
			return Some(Err!("malformed state diff parent"));
		};

		if tail.is_empty() {
			return None;
		}

		if !*removed && tail.starts_with(&0_u64.to_be_bytes()) {
			*removed = true;
			*tail = tail.get(8..).unwrap_or_default();
		}

		// The tail was not empty above, so only a consumed separator empties it here.
		if tail.is_empty() {
			return Some(Err!("empty state diff removed run"));
		}

		let entry = tail
			.get(..16)
			.and_then(|bytes| Some((short_of(bytes.get(..8)?)?, short_of(bytes.get(8..)?)?)))
			.filter(|&(key, event)| key != 0 && event != 0)
			.ok_or_else(|| err!("malformed state diff entry"));

		*tail = tail.get(16..).unwrap_or_default();

		Some(entry)
	})
}

#[implement(References)]
fn retain_event(&mut self, aliases: &BTreeSet<u64>, bytes: &[u8]) {
	match short_of(bytes).filter(|short| *short != 0) {
		| Some(event) => retain(&mut self.events, aliases, event),
		| None => self.event_complete = false,
	}
}

fn retain(references: &mut BTreeSet<u64>, aliases: &BTreeSet<u64>, short: u64) {
	if aliases.contains(&short) {
		references.insert(short);
	}
}
