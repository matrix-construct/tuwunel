use std::{borrow::Borrow, collections::HashMap, mem::take, sync::Arc};

use futures::{
	FutureExt, StreamExt, TryFutureExt, TryStreamExt,
	future::{join, try_join},
};
use ruma::{
	EventId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId,
	events::{StateEventType, TimelineEventType},
	room_version_rules::RoomVersionRules,
};
use tracing::Span;
use tuwunel_core::{
	Result, apply, debug, debug_warn, err, implement,
	matrix::{
		Event, PduEvent, StateKey,
		pdu::PrevEvents,
		room_version::{self, from_create_event},
	},
	trace,
	utils::stream::{BroadbandExt, IterStream, TryWidebandExt},
	warn,
};

use crate::rooms::{
	short::{ShortStateHash, ShortStateKey},
	state_compressor::CompressedState,
	state_res::auth_check,
};

/// State before or after one event, in the shape the sibling builders return.
type StateIds = HashMap<ShortStateKey, OwnedEventId>;

/// Summary of one local build attempt, for the admin debug command.
#[derive(Debug)]
pub struct LocalBuildReport {
	pub state_len: Option<usize>,
	pub visited: usize,
	pub forks: usize,
	pub gate_drops: usize,
	pub memo_hits: usize,
	pub fallback: Option<String>,
}

/// Active writes fork-node memo rows; Shadow suppresses all persistent
/// writes.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum WalkMode {
	Active,
	Shadow,
}

/// State threaded through one walk's discovery and build phases.
struct Walk<'a> {
	room_id: &'a RoomId,
	room_version: &'a RoomVersionId,
	room_rules: RoomVersionRules,
	create_event_id: &'a EventId,
	mode: WalkMode,
	max_nodes: usize,
	top_prevs: PrevEvents,
	class: HashMap<OwnedEventId, Class>,
	nodes: Vec<Node>,
	order: Vec<usize>,
	frontier: HashMap<OwnedEventId, usize>,
	resolved: HashMap<OwnedEventId, Arc<StateIds>>,
	live_entries: usize,
	peak_entries: usize,
	forks: usize,
	gate_drops: usize,
	memo_hits: usize,
	fallback: Option<Fallback>,
}

/// Held outlier in the walk sub-DAG.
struct Node {
	pdu: PduEvent,
	consumers: usize,
}

/// Ancestry classification from the discovery phase.
#[derive(Clone, Copy)]
enum Class {
	/// Committed to the timeline with resolved state at the event.
	Committed(ShortStateHash),

	/// Uncommitted, but an eventid_resolvedstate row exists.
	Memoized,

	/// Uncommitted outlier we hold; the index into Walk::nodes.
	Held(usize),
}

/// Why a walk gave up; every reason falls back to the federation fetch.
#[derive(Clone, Copy)]
enum Fallback {
	Absent,
	Ceiling,
	AuthMissing,
	AllCommitted,
	Entries,
	Canary,
	CreateMismatch,
	Error,
}

/// Ceiling on simultaneously live state-map entries across one walk: the sum
/// of the lengths of materialized maps no consumer has released yet.
/// Exceeding it falls back to the federation fetch. Deliberately a const, not
/// config; revisit only if operation trips it.
const MAX_LIVE_ENTRIES: usize = 1 << 19;

/// Bound on diverging shortstatekeys sampled into the shadow-mode debug log.
const DIVERGENCE_SAMPLE: usize = 16;

/// Build the state before `incoming_pdu` from events we already hold, walking
/// locally held uncommitted ancestry down to committed or memoized ancestors
/// with an auth gate on every folded state event. Some(map) is a complete
/// gated build in the shape the sibling builders return; None falls back to
/// the federation state fetch, for any reason. Err propagates only server
/// shutdown and room-version failures.
#[implement(super::Service)]
pub(super) async fn state_at_incoming_local<Pdu>(
	&self,
	room_id: &RoomId,
	incoming_pdu: &Pdu,
	room_version: &RoomVersionId,
	create_event_id: &EventId,
	mode: WalkMode,
) -> Result<Option<StateIds>>
where
	Pdu: Event,
{
	let top_prevs = incoming_pdu
		.prev_events()
		.map(ToOwned::to_owned)
		.collect();

	let services = self.services.clone();
	let room_id = room_id.to_owned();
	let room_version = room_version.clone();
	let create_event_id = create_event_id.to_owned();
	let parent = Span::current();

	self.services
		.server
		.runtime()
		.spawn(async move {
			services
				.event_handler
				.walk_task(room_id, room_version, create_event_id, mode, top_prevs, parent)
				.await
		})
		.await
		.unwrap_or_else(|error| {
			debug_warn!(
				%error,
				"Local state build task failed; falling back to federation fetch.",
			);

			Ok(None)
		})
}

/// Walk body on its own task: a poll descends every combinator layer from the
/// task root, and under /send intake, already the server's deepest stack, the
/// walk's auth-gate subtree overflows the worker stack in debug builds.
#[implement(super::Service)]
#[tracing::instrument(name = "local", level = "debug", parent = &parent, skip_all)]
async fn walk_task(
	&self,
	room_id: OwnedRoomId,
	room_version: RoomVersionId,
	create_event_id: OwnedEventId,
	mode: WalkMode,
	top_prevs: PrevEvents,
	parent: Span,
) -> Result<Option<StateIds>> {
	let max_nodes = self
		.services
		.server
		.config
		.resolve_state_locally_max;

	let mut walk =
		Walk::new(&room_id, &room_version, &create_event_id, mode, max_nodes, top_prevs)?;

	let state = self.walk_state(&mut walk).await?;

	debug!(
		visited = walk.nodes.len(),
		forks = walk.forks,
		gate_drops = walk.gate_drops,
		memo_hits = walk.memo_hits,
		live_entries_peak = walk.peak_entries,
		outcome = walk.fallback.map_or("resolved", Fallback::name),
		"Local state build finished.",
	);

	if let Some(fallback) = walk.fallback {
		debug_warn!(
			reason = fallback.name(),
			"Local state build falling back to federation fetch.",
		);
	}

	Ok(state)
}

/// Run a read-only (shadow-mode) walk for one stored event and describe the
/// outcome, for the admin debug command.
#[implement(super::Service)]
pub async fn local_state_report(&self, event_id: &EventId) -> Result<LocalBuildReport> {
	let pdu = self.services.timeline.get_pdu(event_id).await?;

	let create_event = self
		.services
		.state_accessor
		.room_state_get(pdu.room_id(), &StateEventType::RoomCreate, "")
		.await?;

	let room_version = from_create_event(&create_event)?;
	let max_nodes = self
		.services
		.server
		.config
		.resolve_state_locally_max;

	let top_prevs = pdu.prev_events().map(ToOwned::to_owned).collect();

	let mut walk = Walk::new(
		pdu.room_id(),
		&room_version,
		create_event.event_id(),
		WalkMode::Shadow,
		max_nodes,
		top_prevs,
	)?;

	let state = self.walk_state(&mut walk).await?;

	Ok(LocalBuildReport {
		state_len: state.map(|state| state.len()),
		visited: walk.nodes.len(),
		forks: walk.forks,
		gate_drops: walk.gate_drops,
		memo_hits: walk.memo_hits,
		fallback: walk
			.fallback
			.map(|fallback| fallback.name().to_owned()),
	})
}

/// Diff a shadow-mode local build against the authoritative fetched state.
/// Divergence is neutral on which side is wrong; the soak analysis decides.
pub(super) fn compare_shadow(
	room_id: &RoomId,
	event_id: &EventId,
	local: &StateIds,
	fetched: &StateIds,
) {
	let only_local: Vec<ShortStateKey> = diverging(local, fetched).collect();
	let only_fetch: Vec<ShortStateKey> = diverging(fetched, local).collect();

	if only_local.is_empty() && only_fetch.is_empty() {
		debug!(%room_id, %event_id, "Shadow local state build matches fetched state.");
		return;
	}

	warn!(
		%room_id,
		%event_id,
		only_local = only_local.len(),
		only_fetch = only_fetch.len(),
		"Shadow local state build diverges from fetched state.",
	);

	let sample: Vec<_> = only_local
		.iter()
		.chain(only_fetch.iter())
		.copied()
		.take(DIVERGENCE_SAMPLE)
		.collect();

	debug!(?sample, "Diverging shortstatekeys.");
}

/// Keys of entries in `a` absent from or differing in `b`.
fn diverging<'a>(a: &'a StateIds, b: &'a StateIds) -> impl Iterator<Item = ShortStateKey> + 'a {
	a.iter()
		.filter(|&(shortstatekey, event_id)| b.get(shortstatekey) != Some(event_id))
		.map(|(&shortstatekey, _)| shortstatekey)
}

/// Drive discovery then the post-order build; any abnormality sets
/// walk.fallback and yields None.
#[implement(super::Service)]
async fn walk_state(&self, walk: &mut Walk<'_>) -> Result<Option<StateIds>> {
	self.walk_discover(walk).await?;

	if walk.fallback.is_some() {
		return Ok(None);
	}

	self.walk_build(walk).await
}

/// Classify the uncommitted ancestry below the incoming event with point
/// reads only, emitting held nodes in post-order; every condition the build
/// cannot survive sets walk.fallback here, before any state materializes.
#[implement(super::Service)]
async fn walk_discover(&self, walk: &mut Walk<'_>) -> Result {
	let mut stack: Vec<(OwnedEventId, bool)> = walk
		.top_prevs
		.iter()
		.map(|prev| (prev.clone(), false))
		.collect();

	while let Some((event_id, expanded)) = stack.pop() {
		self.services.server.check_running()?;

		if expanded {
			// Post-order emission: every prev of this node is fully classified.
			let Some(Class::Held(index)) = walk.class.get(&event_id).copied() else {
				debug_assert!(false, "expanded stack entries are held nodes");
				walk.fallback = Some(Fallback::Error);
				return Ok(());
			};

			walk.order.push(index);
			continue;
		}

		if walk.class.contains_key(&event_id) {
			continue;
		}

		if let Ok(shortstatehash) = self
			.services
			.state
			.pdu_shortstatehash(&event_id)
			.await
		{
			walk.class
				.insert(event_id, Class::Committed(shortstatehash));

			continue;
		}

		if self
			.db
			.eventid_resolvedstate
			.exists(&event_id)
			.await
			.is_ok()
		{
			walk.class.insert(event_id, Class::Memoized);
			continue;
		}

		let Ok(pdu) = self.services.timeline.get_pdu(&event_id).await else {
			trace!(%event_id, "Ancestor is not held locally.");
			walk.fallback = Some(Fallback::Absent);
			return Ok(());
		};

		if walk.nodes.len() >= walk.max_nodes {
			walk.fallback = Some(Fallback::Ceiling);
			return Ok(());
		}

		if pdu.prev_events().next().is_none() {
			debug_warn!(%event_id, "Held uncommitted ancestor has no prev events.");
			walk.fallback = Some(Fallback::Error);
			return Ok(());
		}

		if !self.walk_auth_present(walk, &pdu).await {
			walk.fallback = Some(Fallback::AuthMissing);
			return Ok(());
		}

		walk.class
			.insert(event_id.clone(), Class::Held(walk.nodes.len()));

		stack.push((event_id, true));
		stack.extend(
			pdu.prev_events()
				.map(|prev| (prev.to_owned(), false)),
		);
		walk.nodes.push(Node { pdu, consumers: 0 });
	}

	if walk.nodes.is_empty() {
		// The sibling builders already failed the all-committed shape before
		// the walk ran; re-resolving it would only fail again.
		walk.fallback = Some(Fallback::AllCommitted);
		return Ok(());
	}

	walk.count_consumers();

	Ok(())
}

/// The auth gate must stay evaluable: every auth event of a held node has to
/// be present locally before the walk commits to building through it. Hydra
/// rooms chain the create event implied by the room id.
#[implement(super::Service)]
async fn walk_auth_present(&self, walk: &Walk<'_>, pdu: &PduEvent) -> bool {
	let is_hydra = !walk
		.room_rules
		.event_format
		.allow_room_create_in_auth_events;

	let not_create = *pdu.kind() != TimelineEventType::RoomCreate;
	let hydra_create_id = (not_create && is_hydra)
		.then(|| pdu.room_id().as_event_id().ok())
		.flatten();

	pdu.auth_events()
		.chain(hydra_create_id.as_deref())
		.stream()
		.all(|auth_id| self.services.timeline.pdu_exists(auth_id))
		.await
}

/// Compute state through the walk sub-DAG in post-order, so every node's
/// prevs resolve before it, then combine at the incoming event's own prevs.
#[implement(super::Service)]
async fn walk_build(&self, walk: &mut Walk<'_>) -> Result<Option<StateIds>> {
	let order = take(&mut walk.order);
	for index in order {
		self.services.server.check_running()?;

		if !self.walk_node(walk, index).await {
			return Ok(None);
		}
	}

	let top_prevs = take(&mut walk.top_prevs);
	let state = match top_prevs.as_slice() {
		| [prev] => self.state_after(walk, prev).await,
		| _ => self.fork_resolve(walk, &top_prevs, None).await,
	};

	let Some(state) = state else {
		return Ok(None);
	};

	// Mirror fetch_state's canary: the original create event must still be in
	// the built state.
	let create_entry = self
		.services
		.short
		.get_shortstatekey(&StateEventType::RoomCreate, "")
		.await
		.ok()
		.and_then(|shortstatekey| state.get(&shortstatekey))
		.map(AsRef::as_ref);

	if state.is_empty() || create_entry != Some(walk.create_event_id) {
		walk.fallback = Some(Fallback::CreateMismatch);
		return Ok(None);
	}

	walk.resolved.clear();

	let state = Arc::try_unwrap(state).unwrap_or_else(|state| (*state).clone());

	Ok(Some(state))
}

/// Resolve one held node: state-before from its prevs, its own gated fold on
/// top, retained until the last consumer releases it.
#[implement(super::Service)]
async fn walk_node(&self, walk: &mut Walk<'_>, index: usize) -> bool {
	let node = &walk.nodes[index];
	let event_id = node.pdu.event_id().to_owned();
	let prevs: PrevEvents = node
		.pdu
		.prev_events()
		.map(ToOwned::to_owned)
		.collect();

	let before = match prevs.as_slice() {
		| [prev] => self.state_after(walk, prev).await,
		| _ =>
			self.fork_resolve(walk, &prevs, Some(&event_id))
				.await,
	};

	let Some(before) = before else {
		return false;
	};

	let after = match walk.nodes[index].pdu.state_key() {
		| None => before,
		| Some(_) =>
			self.gated_fold(
				&walk.room_rules,
				&mut walk.gate_drops,
				&walk.nodes[index].pdu,
				&before,
			)
			.await,
	};

	if !walk.retain(event_id, after) {
		return false;
	}

	walk.release(&prevs);

	true
}

/// State after one prev: an already-resolved node or materialized frontier
/// entry shares its map; otherwise the frontier materializes here.
#[implement(super::Service)]
async fn state_after(&self, walk: &mut Walk<'_>, event_id: &EventId) -> Option<Arc<StateIds>> {
	if let Some(state) = walk.resolved.get(event_id) {
		return Some(state.clone());
	}

	let state = match walk.class.get(event_id).copied() {
		| Some(Class::Committed(shortstatehash)) =>
			self.committed_state_after(walk, event_id, shortstatehash)
				.await,
		| Some(Class::Memoized) => self.memoized_state_after(walk, event_id).await,
		| Some(Class::Held(_)) | None => {
			debug_assert!(false, "held nodes resolve before their consumers");
			walk.fallback = Some(Fallback::Error);
			None
		},
	}?;

	walk.retain(event_id.to_owned(), state.clone())
		.then_some(state)
}

/// State after a committed frontier event: its stored state plus its own key
/// folded unguarded, exactly the degree-one builder's shape; a committed
/// event passed full state-dependent auth at its own upgrade.
#[implement(super::Service)]
async fn committed_state_after(
	&self,
	walk: &mut Walk<'_>,
	event_id: &EventId,
	shortstatehash: ShortStateHash,
) -> Option<Arc<StateIds>> {
	let pdu = self.services.timeline.get_pdu(event_id);

	let state = self
		.services
		.state_accessor
		.state_full_ids(shortstatehash)
		.collect::<StateIds>()
		.map(Ok);

	let Ok((pdu, mut state)) = try_join(pdu, state)
		.inspect_err(|e| debug_warn!(%event_id, %e, "Failed loading committed state."))
		.await
	else {
		walk.fallback = Some(Fallback::Error);
		return None;
	};

	if let Some(state_key) = pdu.state_key() {
		let event_type = pdu.event_type().to_cow_str().into();
		let shortstatekey = self
			.services
			.short
			.get_or_create_shortstatekey(&event_type, state_key)
			.await;

		state.insert(shortstatekey, event_id.to_owned());
	}

	Some(Arc::new(state))
}

/// State after a memoized frontier event: the memo row is its state-before
/// (the column's uniform meaning), so its own gated fold recomputes on top.
#[implement(super::Service)]
async fn memoized_state_after(
	&self,
	walk: &mut Walk<'_>,
	event_id: &EventId,
) -> Option<Arc<StateIds>> {
	walk.memo_hits = walk.memo_hits.saturating_add(1);

	let state = self.cached_resolved_state(event_id);

	let pdu = self
		.services
		.timeline
		.get_pdu(event_id)
		.inspect_err(|e| debug_warn!(%event_id, %e, "Failed loading memoized event."));

	let (state, pdu) = join(state, pdu).await;

	let Some(state) = state else {
		walk.fallback = Some(Fallback::Canary);
		return None;
	};

	let Ok(pdu) = pdu else {
		walk.fallback = Some(Fallback::Error);
		return None;
	};

	let before = Arc::new(state);
	if pdu.state_key().is_none() {
		return Some(before);
	}

	let after = self
		.gated_fold(&walk.room_rules, &mut walk.gate_drops, &pdu, &before)
		.await;

	Some(after)
}

/// Fold the event's own state key over its state-before, only when the
/// position-correct auth gate passes; a rejection leaves state unchanged.
/// Discovery pre-verified the auth events exist locally, so a gate error is a
/// deterministic auth verdict, not an unevaluable input.
#[implement(super::Service)]
async fn gated_fold(
	&self,
	room_rules: &RoomVersionRules,
	gate_drops: &mut usize,
	pdu: &PduEvent,
	before: &Arc<StateIds>,
) -> Arc<StateIds> {
	let state_fetch = async |k: StateEventType, s: StateKey| {
		let shortstatekey = self
			.services
			.short
			.get_shortstatekey(&k, s.as_str())
			.await?;

		let event_id = before
			.get(&shortstatekey)
			.ok_or_else(|| err!(Request(NotFound("Not in state before event."))))?;

		self.services.timeline.get_pdu(event_id).await
	};

	let event_fetch = async |event_id: OwnedEventId| self.event_fetch(&event_id).await;

	if let Err(e) = auth_check(room_rules, pdu, &event_fetch, &state_fetch).await {
		debug!(event_id = %pdu.event_id(), %e, "Auth gate rejected fold.");
		*gate_drops = gate_drops.saturating_add(1);
		return before.clone();
	}

	let state_key = pdu.state_key().expect("only state events fold");

	let event_type = pdu.event_type().to_cow_str().into();
	let shortstatekey = self
		.services
		.short
		.get_or_create_shortstatekey(&event_type, state_key)
		.await;

	let mut state = StateIds::clone(before);
	state.insert(shortstatekey, pdu.event_id().to_owned());

	Arc::new(state)
}

/// State before a fork node, resolving the state after each of its prevs
/// exactly as the committed-prev fork resolves today. Fork outputs are the
/// artifacts worth memoizing; chain nodes are cheap to re-derive.
#[implement(super::Service)]
async fn fork_resolve(
	&self,
	walk: &mut Walk<'_>,
	prevs: &[OwnedEventId],
	memo_event_id: Option<&EventId>,
) -> Option<Arc<StateIds>> {
	walk.forks = walk.forks.saturating_add(1);

	// Sequential: materializing a frontier prev writes the walk's accounting.
	let mut afters = Vec::with_capacity(prevs.len());
	for prev in prevs {
		afters.push(self.state_after(walk, prev).await?);
	}

	let (room_id, room_version) = (walk.room_id, walk.room_version);
	let branches = afters
		.iter()
		.try_stream()
		.wide_and_then(async |after| {
			let state: Vec<_> = after
				.iter()
				.map(|(shortstatekey, event_id)| (*shortstatekey, event_id.clone()))
				.collect();

			self.fork_state_and_chain(room_id, room_version, &state)
				.await
		})
		.try_collect()
		.map_ok(Vec::into_iter)
		.map_ok(Iterator::unzip)
		.map_ok(apply!(2, Vec::into_iter))
		.map_ok(apply!(2, IterStream::stream))
		.inspect_err(|e| debug_warn!(%e, "Fork branch failed."))
		.await;

	let Ok((fork_states, auth_chains)) = branches else {
		walk.fallback = Some(Fallback::Error);
		return None;
	};

	let Ok(resolved) = self
		.state_resolution(walk.room_id, walk.room_version, fork_states, auth_chains)
		.await
	else {
		walk.fallback = Some(Fallback::Error);
		return None;
	};

	let state: StateIds = resolved
		.into_iter()
		.stream()
		.broad_then(async |((event_type, state_key), event_id)| {
			self.services
				.short
				.get_or_create_shortstatekey(&event_type, &state_key)
				.map(move |shortstatekey| (shortstatekey, event_id))
				.await
		})
		.collect()
		.await;

	if let Some(event_id) = memo_event_id.filter(|_| walk.mode == WalkMode::Active) {
		let compressed: Arc<CompressedState> = self
			.services
			.state_compressor
			.compress_state_events(
				state
					.iter()
					.map(|(shortstatekey, event_id)| (shortstatekey, event_id.borrow())),
			)
			.collect()
			.map(Arc::new)
			.await;

		self.cache_resolved_state(walk.room_id, event_id, compressed)
			.await;
	}

	Some(Arc::new(state))
}

impl<'a> Walk<'a> {
	fn new(
		room_id: &'a RoomId,
		room_version: &'a RoomVersionId,
		create_event_id: &'a EventId,
		mode: WalkMode,
		max_nodes: usize,
		top_prevs: PrevEvents,
	) -> Result<Self> {
		Ok(Self {
			room_id,
			room_version,
			room_rules: room_version::rules(room_version)?,
			create_event_id,
			mode,
			max_nodes,
			top_prevs,
			class: HashMap::new(),
			nodes: Vec::new(),
			order: Vec::new(),
			frontier: HashMap::new(),
			resolved: HashMap::new(),
			live_entries: 0,
			peak_entries: 0,
			forks: 0,
			gate_drops: 0,
			memo_hits: 0,
			fallback: None,
		})
	}

	/// Consumer counts drive state-map reaping: each held node's prevs and
	/// the incoming event's own prevs each count one consumption.
	fn count_consumers(&mut self) {
		let mut held = vec![0_usize; self.nodes.len()];

		let edges = self
			.nodes
			.iter()
			.flat_map(|node| node.pdu.prev_events())
			.chain(self.top_prevs.iter().map(AsRef::as_ref));

		for prev in edges {
			match self.class.get(prev).copied() {
				| Some(Class::Held(index)) => held[index] = held[index].saturating_add(1),
				| Some(_) => {
					let consumers = self.frontier.entry(prev.to_owned()).or_default();

					*consumers = consumers.saturating_add(1);
				},
				| None => debug_assert!(false, "every walk edge is classified"),
			}
		}

		for (node, consumers) in self.nodes.iter_mut().zip(held) {
			node.consumers = consumers;
		}
	}

	/// Retain a computed state map until its last consumer releases it; the
	/// running live-entry total is the walk's memory ceiling. Arc-shared maps
	/// count once per holder, deliberately over-counting toward the ceiling.
	fn retain(&mut self, event_id: OwnedEventId, state: Arc<StateIds>) -> bool {
		let live_entries = self.live_entries.saturating_add(state.len());
		if live_entries > MAX_LIVE_ENTRIES {
			self.fallback = Some(Fallback::Entries);
			return false;
		}

		self.live_entries = live_entries;
		self.peak_entries = self.peak_entries.max(live_entries);
		self.resolved.insert(event_id, state);

		true
	}

	/// Release one consumption of each prev, dropping maps no consumer
	/// awaits.
	fn release(&mut self, prevs: &[OwnedEventId]) {
		for prev in prevs {
			let remaining = match self.class.get(prev).copied() {
				| Some(Class::Held(index)) => {
					let node = &mut self.nodes[index];
					node.consumers = node.consumers.saturating_sub(1);
					node.consumers
				},
				| _ => {
					let Some(consumers) = self.frontier.get_mut(prev) else {
						continue;
					};

					*consumers = consumers.saturating_sub(1);
					*consumers
				},
			};

			if remaining == 0
				&& let Some(state) = self.resolved.remove(prev)
			{
				self.live_entries = self.live_entries.saturating_sub(state.len());
			}
		}
	}
}

impl Fallback {
	fn name(self) -> &'static str {
		match self {
			| Self::Absent => "absent",
			| Self::Ceiling => "ceiling",
			| Self::AuthMissing => "auth_missing",
			| Self::AllCommitted => "all_committed",
			| Self::Entries => "entries",
			| Self::Canary => "canary",
			| Self::CreateMismatch => "create_mismatch",
			| Self::Error => "error",
		}
	}
}
