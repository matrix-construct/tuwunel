use std::{
	collections::{BTreeMap, BTreeSet},
	io::Error as IoError,
	iter::{once, repeat_n},
	str::from_utf8,
};

use futures::{FutureExt, TryFutureExt, TryStreamExt, stream::unfold};
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId, api::error::ErrorKind};
use serde::Deserialize;
use tuwunel_core::{
	Error, Result,
	utils::{
		IterStream, OptionExt, TryReadyExt,
		result::NotFound,
		stream::{TryBroadbandExt, TryWidebandExt},
	},
};
use tuwunel_database::{Database, keyval::KeyBuf};
use tuwunel_matrix::{
	PduEvent,
	pdu::AuthEvents,
	room::state::resolution::{AuthCheckOutcome, FetchEvent, StateMap, auth_check},
	room_version::{from_create_event, rules},
};

use super::{
	CompressedState, Diff, Identity, Mapping, Materialized, Projection, Services, apply, claim,
	identity, layer, lookup, materialize, project, project_entry, row, short_of, statekey,
	visit as visit_state,
};
use crate::{
	migrations::scan::ScanExt,
	rooms::state_compressor::{
		CompressedStateEvent, compress_state_event, parse_compressed_state_event,
	},
};

type Anchors = BTreeMap<u64, Vec<u64>>;
type Auth = BTreeMap<OwnedEventId, AuthEvents>;
type Dependents<'a> = BTreeMap<&'a OwnedEventId, Vec<&'a OwnedEventId>>;
type Events = BTreeSet<OwnedEventId>;
type Step = (PduEvent, Option<CompressedState>);
type Steps = Vec<Option<Step>>;
type Boundary = (PduEvent, StateMap<PduEvent>);

const EVENTS: (&str, &str) = ("eventid_shorteventid", "shorteventid_eventid");
const STATEKEYS: (&str, &str) = ("statekey_shortstatekey", "shortstatekey_statekey");

#[derive(Clone, Copy)]
struct Accepted<'a>(&'a Services);

pub(super) struct References {
	pub(super) affected: BTreeSet<u64>,
	anchors: Anchors,
	held: BTreeSet<u64>,
	unreadable: Vec<(&'static str, KeyBuf)>,
	rooms: BTreeSet<OwnedRoomId>,
	complete: bool,
}

#[derive(Deserialize)]
struct Located {
	room_id: OwnedRoomId,
}

impl FetchEvent for Accepted<'_> {
	async fn get<T>(self, event_id: &EventId) -> Result<T>
	where
		T: for<'de> Deserialize<'de> + Send,
	{
		self.0
			.timeline
			.get_non_outlier(event_id)
			.map_err(read_error)
			.await
	}

	async fn exists(self, event_id: &EventId) -> Result<bool> {
		self.0
			.timeline
			.non_outlier_pdu_exists(event_id)
			.map_ok(|()| true)
			.map_err(read_error)
			.await
	}
}

// Rewrap storage failures so auth cannot read them as missing or invalid evidence.
pub(super) fn read_error(error: Error) -> Error {
	match error {
		| error @ Error::Err(..) => IoError::other(error).into(),
		| error @ Error::Io(..) if error.is_not_found() => IoError::other(error).into(),
		| error => error,
	}
}

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn references(
	services: &Services,
	affected: BTreeSet<u64>,
) -> Result<References> {
	let initial = References {
		affected,
		anchors: Anchors::new(),
		held: BTreeSet::new(),
		unreadable: Vec::new(),
		rooms: BTreeSet::new(),
		complete: true,
	};

	if initial.affected.is_empty() {
		return Ok(initial);
	}

	["shorteventid_shortstatehash", "roomid_shortstatehash", "eventid_resolvedstate"]
		.try_stream()
		.try_fold(initial, |references, column| scan(services, references, column))
		.and_then(|references| locate(services, references))
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn scan(
	services: &Services,
	references: References,
	column: &'static str,
) -> Result<References> {
	services.db[column]
		.raw_stream()
		.scanned(&services.server)
		.ready_try_fold(references, |references, (key, value)| {
			Ok(reference(references, column, key, value))
		})
		.await
}

fn reference(
	references: References,
	column: &'static str,
	key: &[u8],
	value: &[u8],
) -> References {
	let Some(id) = claim(value) else {
		return unreadable(references, column, key);
	};

	if !references.affected.contains(&id) {
		return references;
	}

	match column {
		| _ if column != "shorteventid_shortstatehash" => held(references, id),
		| _ => match short_of(key) {
			| None => held(references, id),
			| Some(event) => anchored(references, id, event),
		},
	}
}

fn unreadable(mut references: References, column: &'static str, key: &[u8]) -> References {
	references
		.unreadable
		.push((column, KeyBuf::from_slice(key)));

	references
}

fn held(mut references: References, id: u64) -> References {
	references.held.insert(id);
	references
}

fn anchored(mut references: References, id: u64, event: u64) -> References {
	references
		.anchors
		.entry(id)
		.or_default()
		.push(event);

	references
}

#[tracing::instrument(level = "debug", skip_all)]
async fn locate(services: &Services, references: References) -> Result<References> {
	let rooms = references
		.unreadable
		.iter()
		.try_stream()
		.broad_and_then(async |(column, key)| room(services, column, key).await)
		.ready_try_fold(Some(BTreeSet::new()), |rooms, room| {
			Ok(rooms.zip(room).map(|(mut rooms, room)| {
				rooms.insert(room);
				rooms
			}))
		})
		.await?;

	Ok(match rooms {
		| Some(rooms) => References { rooms, ..references },
		| None => incomplete(references),
	})
}

#[tracing::instrument(level = "trace", skip_all)]
async fn room(services: &Services, column: &str, key: &[u8]) -> Result<Option<OwnedRoomId>> {
	let db = &services.db;
	let event = match column {
		| "roomid_shortstatehash" => {
			return Ok(from_utf8(key)
				.ok()
				.and_then(|room| RoomId::parse(room).ok()));
		},
		| "eventid_resolvedstate" => Some(Identity::from_slice(key)),
		| _ => short_of(key)
			.map_async(async |short| {
				lookup(db, "shorteventid_eventid", &short.to_be_bytes()).await
			})
			.await
			.transpose()?
			.flatten(),
	};

	let Some(event) = event else {
		return Ok(None);
	};

	match lookup(db, "eventid_pduid", &event).await? {
		| Some(pdu_id) => located(db, "pduid_pdu", &pdu_id).await,
		| None => located(db, "eventid_outlierpdu", &event).await,
	}
}

async fn located(db: &Database, map: &str, key: &[u8]) -> Result<Option<OwnedRoomId>> {
	db[map]
		.get(key)
		.map_ok(|pdu| {
			serde_json::from_slice(&pdu)
				.ok()
				.map(|located: Located| located.room_id)
		})
		.map(NotFound::present)
		.map_ok(Option::flatten)
		.await
}

fn incomplete(references: References) -> References {
	References { complete: false, ..references }
}

#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn target(
	services: &Services,
	projection: &Projection,
	references: &References,
	id: u64,
) -> Result<Materialized> {
	if !references.complete || references.held.contains(&id) {
		return Ok(None);
	}

	let Some(anchors) = references
		.anchors
		.get(&id)
		.filter(|anchors| !anchors.is_empty())
	else {
		return Ok(None);
	};

	let Some(original) = row(&services.db, id).await? else {
		return Ok(None);
	};

	let (first, agreed) = anchors
		.try_stream()
		.try_fold((None, true), async |(first, agreed), anchor| -> Result<_> {
			// Once anchors disagree the row stays withheld; later anchors are never read.
			if !agreed {
				return Ok((first, agreed));
			}

			let next = reconstruct(services, projection, references, *anchor, id).await?;
			let agreed = next.is_some()
				&& first
					.as_ref()
					.is_none_or(|first| Some(first) == next.as_ref());

			Ok((first.or(next), agreed))
		})
		.await?;

	let Some(first) = first.filter(|_| agreed) else {
		return Ok(None);
	};

	let target = intact(services, projection, id, &first)
		.await?
		.then_some((original, first));

	Ok(target)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn reconstruct(
	services: &Services,
	projection: &Projection,
	references: &References,
	anchor: u64,
	id: u64,
) -> Result<Option<CompressedState>> {
	let db = &services.db;
	let Some((_, event)) =
		identity(db, &projection.events, anchor, "shorteventid_eventid").await?
	else {
		return Ok(None);
	};

	let Some(anchor) = accepted(services, &event).await? else {
		return Ok(None);
	};

	// An unreadable referrer in this room may name this state.
	if references.rooms.contains(&anchor.room_id) {
		return Ok(None);
	}

	if pointer(services, projection, &anchor).await? != Some(id) {
		return Ok(None);
	}

	let steps = unfold((Some(anchor.clone()), Events::new()), async |(current, visited)| {
		let current = current?;
		let step = predecessor(services, projection, references, &current, &visited).await;
		let next = step
			.as_ref()
			.ok()
			.and_then(Option::as_ref)
			.filter(|(_, boundary)| boundary.is_none())
			.map(|(pdu, _)| pdu.clone());

		Some((step, (next, visit(visited, current.event_id))))
	})
	.try_collect()
	.await?;

	let Some((base, steps)) = boundary(steps) else {
		return Ok(None);
	};

	let Some((create, state)) = validate(services, projection, base, &anchor.room_id).await?
	else {
		return Ok(None);
	};

	let target = steps
		.try_stream()
		.try_fold(Some(state), async |state, pdu| -> Result<_> {
			let Some(state) = state else {
				return Ok(None);
			};

			let authorized = authorized(services, &pdu, &create, &state).await?;

			Ok(authorized.then(|| state_row(state, pdu)))
		})
		.await?;

	let Some(target) = target else {
		return Ok(None);
	};

	if !authorized(services, &anchor, &create, &target).await? {
		return Ok(None);
	}

	let compressed = target
		.values()
		.try_stream()
		.broad_and_then(async |pdu| entry_for(services, projection, pdu).await)
		.ready_try_fold(Some(CompressedState::new()), |state, entry| {
			Ok(state.zip(entry).map(|(mut state, entry)| {
				state.insert(entry);
				state
			}))
		})
		.await?;

	Ok(compressed.and_then(|state| project(state, projection)))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn accepted(services: &Services, event: &[u8]) -> Result<Option<PduEvent>> {
	services.server.check_running()?;
	let db = &services.db;
	let Some(pdu_id) = lookup(db, "eventid_pduid", event)
		.map_ok(|pdu_id| pdu_id.filter(|pdu_id| matches!(pdu_id.len(), 16 | 24)))
		.await?
	else {
		return Ok(None);
	};

	let Some(pdu) = db["pduid_pdu"]
		.get(&pdu_id)
		.map_ok(|pdu| serde_json::from_slice(&pdu).ok())
		.map(NotFound::present)
		.map_ok(Option::flatten)
		.map_ok(|pdu| pdu.filter(|pdu: &PduEvent| pdu.event_id.as_bytes() == event))
		.await?
	else {
		// A mismatched row is refused before the room read, which cannot change that.
		return Ok(None);
	};

	let valid = lookup(db, "roomid_shortroomid", pdu.room_id.as_bytes())
		.map_ok(|room| room.as_deref().and_then(short_of) == claim(&pdu_id))
		.await?;

	Ok(valid.then_some(pdu))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn pointer(
	services: &Services,
	projection: &Projection,
	pdu: &PduEvent,
) -> Result<Option<u64>> {
	let db = &services.db;
	let event_id = pdu.event_id.as_bytes();
	let Some(short) = canonical(db, &projection.events, EVENTS, event_id).await? else {
		return Ok(None);
	};

	let Some(id) = lookup(db, "shorteventid_shortstatehash", &short.to_be_bytes())
		.map_ok(|value| value.as_deref().and_then(short_of))
		.await?
	else {
		return Ok(None);
	};

	// A disagreeing alias settles the decline; a read still pending then cannot change it.
	let agree = projection
		.events
		.aliases
		.iter()
		.filter(|(_, (_, identity))| identity.as_slice() == event_id)
		.try_stream()
		.broad_try_all(async |(alias, _)| {
			lookup(db, "shorteventid_shortstatehash", &alias.to_be_bytes())
				.map_ok(|pointer| {
					pointer
						.as_deref()
						.is_none_or(|value| short_of(value) == Some(id))
				})
				.await
		})
		.await?;

	Ok(agree.then_some(id))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn predecessor(
	services: &Services,
	projection: &Projection,
	references: &References,
	current: &PduEvent,
	visited: &Events,
) -> Result<Option<Step>> {
	let [previous] = current.prev_events.as_slice() else {
		return Ok(None);
	};

	if visited.contains(previous) || previous == &current.event_id {
		return Ok(None);
	}

	let Some(previous) = accepted(services, previous.as_bytes()).await? else {
		return Ok(None);
	};

	if previous.room_id != current.room_id {
		return Ok(None);
	}

	let Some(shortstatehash) = pointer(services, projection, &previous).await? else {
		return Ok(None);
	};

	if references.affected.contains(&shortstatehash) {
		return Ok(Some((previous, None)));
	}

	let base = materialize(services, shortstatehash)
		.map_ok(|materialized| materialized.and_then(|(_, state)| project(state, projection)))
		.await?;

	Ok(base.map(|base| (previous, Some(base))))
}

fn visit(mut visited: Events, event: OwnedEventId) -> Events {
	visited.insert(event);
	visited
}

fn boundary(mut steps: Steps) -> Option<(CompressedState, impl Iterator<Item = PduEvent>)> {
	if steps.iter().any(Option::is_none) {
		return None;
	}

	let (pdu, base) = steps.pop()??;

	let predecessors = once(pdu).chain(
		steps
			.into_iter()
			.rev()
			.flatten()
			.map(|(pdu, _)| pdu),
	);

	base.map(|base| (base, predecessors))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn validate(
	services: &Services,
	projection: &Projection,
	base: CompressedState,
	room: &RoomId,
) -> Result<Option<Boundary>> {
	// A rejected entry stops the fold as Err(None), apart from a read failure in Err(Some).
	let loaded = base
		.try_stream()
		.wide_and_then(async |entry| checked_entry(services, projection, room, entry).await)
		.map_err(Some)
		.ready_try_fold(StateMap::new(), |state, pdu| {
			pdu.map(|pdu| state_row(state, pdu)).ok_or(None)
		})
		.await;

	let state = match loaded {
		| Ok(state) => state,
		| Err(None) => return Ok(None),
		| Err(Some(error)) => return Err(error),
	};

	let Some(create) = state
		.get(&("m.room.create".into(), "".into()))
		.filter(|create| create.prev_events.is_empty() && create.auth_events.is_empty())
		.cloned()
	else {
		return Ok(None);
	};

	let permitted = permits(services, &create, &create, &StateMap::new()).await?;

	Ok(permitted.then_some((create, state)))
}

#[tracing::instrument(level = "trace", skip_all)]
async fn checked_entry(
	services: &Services,
	projection: &Projection,
	room: &RoomId,
	entry: CompressedStateEvent,
) -> Result<Option<PduEvent>> {
	let (_, event) = parse_compressed_state_event(entry);
	let Some((_, identity)) =
		identity(&services.db, &projection.events, event, "shorteventid_eventid").await?
	else {
		return Ok(None);
	};

	let Some(pdu) = accepted(services, &identity).await? else {
		return Ok(None);
	};

	let valid = pdu.room_id.as_str() == room.as_str()
		&& entry_for(services, projection, &pdu).await? == Some(entry);

	Ok(valid.then_some(pdu))
}

fn state_row(mut state: StateMap<PduEvent>, pdu: PduEvent) -> StateMap<PduEvent> {
	if let Some(key) = pdu.state_key.as_ref() {
		state.insert((pdu.kind.to_cow_str().as_ref().into(), key.clone()), pdu);
	}

	state
}

#[tracing::instrument(level = "trace", skip_all)]
async fn permits(
	services: &Services,
	pdu: &PduEvent,
	create: &PduEvent,
	state: &StateMap<PduEvent>,
) -> Result<bool> {
	let Ok(rules) = from_create_event(create).and_then(|version| rules(&version)) else {
		return Ok(false);
	};

	auth_check(&rules, pdu, Accepted(services), state)
		.map(allowed)
		.await
}

pub(super) fn allowed(result: Result<AuthCheckOutcome>) -> Result<bool> {
	match result {
		| Ok(outcome) => Ok(matches!(outcome, AuthCheckOutcome::Allow)),
		| Err(
			Error::Database(..)
			| Error::Json(..)
			| Error::SerdeDe(..)
			| Error::Request(ErrorKind::BadJson | ErrorKind::NotFound, ..),
		) => Ok(false),
		| Err(error) => Err(error),
	}
}

#[tracing::instrument(level = "trace", skip_all)]
async fn authorized(
	services: &Services,
	pdu: &PduEvent,
	create: &PduEvent,
	state: &StateMap<PduEvent>,
) -> Result<bool> {
	let authorized = authenticated(services, pdu, create).await?
		&& permits(services, pdu, create, state).await?;

	Ok(authorized)
}

#[tracing::instrument(level = "trace", skip_all)]
async fn entry_for(
	services: &Services,
	projection: &Projection,
	pdu: &PduEvent,
) -> Result<Option<CompressedStateEvent>> {
	let Some(key) = pdu.state_key.as_ref() else {
		return Ok(None);
	};

	let db = &services.db;
	let Some(shortstatekey) =
		canonical(db, &projection.statekeys, STATEKEYS, &statekey(&pdu.kind, key)).await?
	else {
		return Ok(None);
	};

	canonical(db, &projection.events, EVENTS, pdu.event_id.as_bytes())
		.map_ok(|event| event.map(|event| compress_state_event(shortstatekey, event)))
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn canonical(
	db: &Database,
	mapping: &Mapping,
	(forward, reverse): (&str, &str),
	name: &[u8],
) -> Result<Option<u64>> {
	let Some(short) = lookup(db, forward, name)
		.map_ok(|value| value.as_deref().and_then(short_of))
		.await?
	else {
		return Ok(None);
	};

	identity(db, mapping, short, reverse)
		.map_ok(|checked| {
			checked
				.filter(|(winner, identity)| *winner == short && identity.as_slice() == name)
				.map(|(short, _)| short)
		})
		.await
}

#[tracing::instrument(level = "trace", skip_all)]
async fn authenticated(services: &Services, pdu: &PduEvent, create: &PduEvent) -> Result<bool> {
	let pending = [pdu.event_id.clone(), create.event_id.clone()].into();
	let walk = unfold((pending, Events::new()), async |(pending, visited)| {
		let (event, pending) = pending_event(pending)?;
		let fetched = accepted(services, event.as_bytes())
			.map_ok(|pdu| pdu.filter(|pdu| admissible(pdu, create)))
			.await;

		let Ok(Some(event)) = fetched else {
			return Some((fetched.map(|_| None), (Events::new(), visited)));
		};

		let visited = visit(visited, event.event_id.clone());
		let pending = pending_auth(pending, &visited, &event);

		Some((Ok(Some((event.event_id, event.auth_events))), (pending, visited)))
	});

	let graph = walk
		.ready_try_fold(Some(Auth::new()), |graph, item| {
			Ok(graph
				.zip(item)
				.map(|(graph, (event, dependencies))| auth_row(graph, event, dependencies)))
		})
		.await?;

	Ok(graph.as_ref().is_some_and(acyclic))
}

fn pending_event(mut pending: Events) -> Option<(OwnedEventId, Events)> {
	let event = pending.pop_first()?;

	Some((event, pending))
}

fn admissible(event: &PduEvent, create: &PduEvent) -> bool {
	event.room_id == create.room_id
		&& (event.kind.to_cow_str() != "m.room.create"
			|| (event.event_id == create.event_id
				&& event.state_key.as_deref() == Some("")
				&& event.prev_events.is_empty()
				&& event.auth_events.is_empty()))
}

fn pending_auth(mut pending: Events, visited: &Events, pdu: &PduEvent) -> Events {
	pending.extend(
		pdu.auth_events
			.iter()
			.filter(|event| !visited.contains(*event))
			.cloned(),
	);

	pending
}

fn auth_row(mut graph: Auth, event: OwnedEventId, dependencies: AuthEvents) -> Auth {
	graph.insert(event, dependencies);
	graph
}

fn acyclic(graph: &Auth) -> bool {
	let dependents: Dependents<'_> = graph
		.iter()
		.flat_map(|(id, auth)| {
			auth.iter()
				.map(move |dependency| (dependency, id))
		})
		.fold(BTreeMap::new(), |mut dependents, (dependency, id)| {
			dependents.entry(dependency).or_default().push(id);
			dependents
		});

	let pending: BTreeMap<&OwnedEventId, usize> = graph
		.iter()
		.map(|(id, auth)| (id, auth.len()))
		.collect();

	let ready: Vec<&OwnedEventId> = pending
		.iter()
		.filter(|(_, count)| **count == 0)
		.map(|(id, _)| *id)
		.collect();

	// Kahn's walk: each resolved event releases the events depending on it.
	let resolved = repeat_n((), graph.len())
		.scan((pending, ready), |(pending, ready), ()| {
			let id = ready.pop()?;

			dependents
				.get(id)
				.into_iter()
				.flatten()
				.filter(|dependent| released(pending, dependent))
				.for_each(|dependent| ready.push(*dependent));

			Some(())
		})
		.count();

	resolved == graph.len()
}

fn released(pending: &mut BTreeMap<&OwnedEventId, usize>, dependent: &OwnedEventId) -> bool {
	pending.get_mut(dependent).is_some_and(|count| {
		*count = count.saturating_sub(1);
		*count == 0
	})
}

#[tracing::instrument(level = "trace", skip_all)]
async fn intact(
	services: &Services,
	projection: &Projection,
	id: u64,
	target: &CompressedState,
) -> Result<bool> {
	let chain = unfold((Some(id), BTreeSet::new()), async |(id, visited)| {
		let id = id?;
		let diff = layer(&services.db, id)
			.map_ok(|diff| diff.filter(|_| !visited.contains(&id)))
			.await;

		let parent = diff
			.as_ref()
			.ok()
			.and_then(Option::as_ref)
			.and_then(|diff| diff.parent);

		Some((diff, (parent, visit_state(visited, id))))
	})
	.ready_try_fold(Some(Vec::new()), |chain, diff| {
		Ok(chain.zip(diff).map(|(mut chain, diff)| {
			chain.push(diff);
			chain
		}))
	})
	.await?;

	let Some(diffs) = chain else {
		return Ok(false);
	};

	let Some(damaged) = diffs
		.iter()
		.rev()
		.try_fold(BTreeSet::new(), |damaged, diff| damage(damaged, diff, projection))
	else {
		return Ok(false);
	};

	let original = diffs
		.into_iter()
		.rev()
		.fold(CompressedState::new(), apply);

	let Some(original) = project(original, projection) else {
		return Ok(false);
	};

	let unaffected = |entry: &&CompressedStateEvent| {
		!damaged.contains(&parse_compressed_state_event(**entry).0)
	};

	let agrees = original
		.iter()
		.filter(unaffected)
		.eq(target.iter().filter(unaffected));

	Ok(agrees)
}

fn damage(damaged: BTreeSet<u64>, diff: &Diff, projection: &Projection) -> Option<BTreeSet<u64>> {
	// A root's reader ignores removed entries, so only a child's overlap is damage.
	let overlap = diff
		.added
		.intersection(&diff.removed)
		.filter(|_| diff.parent.is_some());

	let damaged = keys(overlap, projection).try_fold(damaged, |mut damaged, key| {
		damaged.insert(key?);
		Some(damaged)
	})?;

	let written = diff.added.symmetric_difference(&diff.removed);

	keys(written, projection).try_fold(damaged, |mut damaged, key| {
		damaged.remove(&key?);
		Some(damaged)
	})
}

fn keys<'a, I>(entries: I, projection: &Projection) -> impl Iterator<Item = Option<u64>>
where
	I: Iterator<Item = &'a CompressedStateEvent>,
{
	entries.map(move |entry| {
		project_entry(*entry, projection).map(|entry| parse_compressed_state_event(entry).0)
	})
}
