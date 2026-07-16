use std::{
	collections::{HashMap as Map, hash_map::Entry},
	iter::once,
	ops::Deref,
};

use futures::{
	Stream, StreamExt,
	stream::{FuturesUnordered, unfold},
};
use ruma::OwnedEventId;
use tuwunel_core::{
	Result, implement, is_equal_to,
	itertools::Itertools,
	matrix::{Event, pdu::AuthEvents},
	smallvec::SmallVec,
	utils::{
		BoolExt,
		stream::{IterStream, automatic_width},
	},
};

#[derive(Default, Debug)]
struct Global<Fut: Future + Send> {
	subgraph: Subgraph,
	todo: Todo<Fut>,
	iter: usize,
}

#[derive(Default, Debug)]
struct Local {
	id: usize,
	path: Path,
	stack: Stack,
}

#[derive(Default, Debug)]
struct Substate {
	subgraph: bool,
	seen: bool,
}

type Todo<Fut> = FuturesUnordered<Fut>;
type Subgraph = Map<OwnedEventId, Substate>;
type Path = SmallVec<[OwnedEventId; PATH_INLINE]>;
type Stack = SmallVec<[Frame; STACK_INLINE]>;
type Frame = AuthEvents;

const PATH_INLINE: usize = 32;
const STACK_INLINE: usize = 32;
const CAPACITY_MULTIPLIER: usize = 4;

#[tracing::instrument(
	name = "subgraph_dfs",
	level = "debug",
	skip_all,
	fields(
		starting_events = %conflicted_set.len(),
	)
)]
pub(super) fn conflicted_subgraph_dfs<Fetch, Fut, Pdu>(
	conflicted_set: &Vec<&OwnedEventId>,
	fetch: &Fetch,
) -> impl Stream<Item = OwnedEventId> + Send
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let initial_capacity = conflicted_set
		.len()
		.saturating_mul(CAPACITY_MULTIPLIER);

	let state = Global {
		subgraph: Map::with_capacity(initial_capacity),
		todo: Todo::<_>::new(),
		iter: 0,
	};

	let inputs = conflicted_set
		.iter()
		.map(Deref::deref)
		.cloned()
		.enumerate()
		.map(Local::new)
		.filter_map(Local::pop)
		.map(|(local, event_id)| local.push(fetch, Some(event_id)));

	unfold((inputs, state), async |(mut inputs, mut state)| {
		debug_assert!(
			state.todo.len() <= automatic_width(),
			"Excessive items todo in FuturesUnordered"
		);

		while state.todo.len() < automatic_width()
			&& let Some(input) = inputs.next()
		{
			state.todo.push(input);
		}

		let outputs = state
			.todo
			.next()
			.await?
			.pop()
			.map(|(local, event_id)| local.eval(&mut state, conflicted_set, event_id))
			.map(|(local, next_id, outputs)| {
				if !local.stack.is_empty() {
					state.todo.push(local.push(fetch, next_id));
				}

				outputs
			})
			.into_iter()
			.flatten()
			.stream();

		state.iter = state.iter.saturating_add(1);
		Some((outputs, (inputs, state)))
	})
	.flatten()
}

#[implement(Local)]
#[tracing::instrument(
	name = "descent",
	level = "trace",
	skip_all,
	fields(
		i = state.iter,
		s = ?state
			.subgraph
			.values()
			.fold((0_u64, 0_u64), |(a, b), v| {
				(a.saturating_add(u64::from(v.subgraph)), b.saturating_add(u64::from(v.seen)))
			}),

		%event_id,
		id = self.id,
		path = self.path.len(),
		stack = self.stack.iter().flatten().count(),
	)
)]
fn eval<Fut: Future + Send>(
	mut self,
	state: &mut Global<Fut>,
	conflicted_event_ids: &Vec<&OwnedEventId>,
	event_id: OwnedEventId,
) -> (Self, Option<OwnedEventId>, Path) {
	let Global { subgraph, .. } = state;

	let insert_path_filter = |subgraph: &mut Subgraph, event_id: &OwnedEventId| match subgraph
		.entry(event_id.clone())
	{
		| Entry::Occupied(state) if state.get().subgraph => false,
		| Entry::Occupied(mut state) => {
			state.get_mut().subgraph = true;
			state.get().subgraph
		},
		| Entry::Vacant(state) =>
			state
				.insert(Substate { subgraph: true, seen: false })
				.subgraph,
	};

	let insert_path = |subgraph: &mut Subgraph, local: &Local| {
		local
			.path
			.iter()
			.filter(|&event_id| insert_path_filter(subgraph, event_id))
			.cloned()
			.collect()
	};

	let is_conflicted = |event_id: &OwnedEventId| {
		conflicted_event_ids
			.binary_search(&event_id)
			.is_ok()
	};

	let mut entry = subgraph.entry(event_id.clone());

	if let Entry::Occupied(state) = &entry
		&& state.get().subgraph
	{
		let path = (self.path.len() > 1)
			.then(|| insert_path(subgraph, &self))
			.unwrap_or_default();

		self.path.pop();
		return (self, None, path);
	}

	if let Entry::Occupied(state) = &mut entry {
		state.get_mut().seen = true;
		return (self, None, Path::new());
	}

	if let Entry::Vacant(state) = entry {
		state.insert(Substate { subgraph: false, seen: true });
	}

	let path = (self.path.len() > 1)
		.and_if(|| is_conflicted(&event_id))
		.then(|| insert_path(subgraph, &self))
		.unwrap_or_default();

	let next_id = self
		.path
		.iter()
		.dropping_back(1)
		.any(is_equal_to!(&event_id))
		.is_false()
		.then_some(event_id);

	(self, next_id, path)
}

#[implement(Local)]
async fn push<Fetch, Fut, Pdu>(mut self, fetch: &Fetch, event_id: Option<OwnedEventId>) -> Self
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	if let Some(event_id) = event_id
		&& let Ok(event) = fetch(event_id).await
	{
		self.stack
			.push(event.auth_events_into().into_iter().collect());
	}

	self
}

#[implement(Local)]
fn pop(mut self) -> Option<(Self, OwnedEventId)> {
	while self.stack.last().is_some_and(Frame::is_empty) {
		self.stack.pop();
		self.path.pop();
	}

	self.stack
		.last_mut()
		.and_then(Frame::pop)
		.inspect(|event_id| self.path.push(event_id.clone()))
		.map(move |event_id| (self, event_id))
}

#[implement(Local)]
fn new((id, conflicted_event_id): (usize, OwnedEventId)) -> Self {
	Self {
		id,
		path: once(conflicted_event_id.clone()).collect(),
		stack: once(once(conflicted_event_id).collect()).collect(),
		..Default::default()
	}
}
