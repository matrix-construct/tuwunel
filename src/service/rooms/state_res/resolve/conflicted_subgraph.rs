use std::{collections::HashMap, iter::once, ops::Deref};

use futures::{
	Stream, StreamExt,
	stream::{FuturesUnordered, unfold},
};
use ruma::{EventId, OwnedEventId};
use tuwunel_core::{
	Result, implement, is_equal_to,
	matrix::{Event, event_id::RandomState, pdu::AuthEvents},
	smallvec::SmallVec,
	utils::{
		BoolExt,
		stream::{IterStream, automatic_width},
	},
};

struct Global<Fut: Future + Send> {
	subgraph: Subgraph,
	todo: Todo<Fut>,
	locals: Locals,
}

#[derive(Debug, Default)]
struct Local {
	path: Path,
	stack: Stack,
	marked: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct Substate {
	subgraph: bool,
	seen: bool,
}

type Todo<Fut> = FuturesUnordered<Fut>;
type Subgraph = HashMap<OwnedEventId, Substate, RandomState>;
type Locals = Vec<Local>;
type Path = SmallVec<[OwnedEventId; PATH_INLINE]>;
type Stack = SmallVec<[Frame; STACK_INLINE]>;
type Frame = AuthEvents;

const PATH_INLINE: usize = 4;
const STACK_INLINE: usize = 4;
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

	let seeds = || conflicted_set.iter().map(Deref::deref).cloned();

	// Seeding the conflicted set makes membership a free by-product of the
	// entry each descent step already reads.
	let mut subgraph = Subgraph::with_capacity_and_hasher(initial_capacity, RandomState);
	subgraph.extend(seeds().map(|event_id| (event_id, Substate::default())));

	let state = Global {
		subgraph,
		todo: Todo::new(),
		locals: Locals::with_capacity(conflicted_set.len()),
	};

	unfold((seeds(), state), async |(mut inputs, mut state)| {
		debug_assert!(
			state.todo.len() <= automatic_width(),
			"Excessive items todo in FuturesUnordered"
		);

		while state.todo.len() < automatic_width()
			&& let Some(seed) = inputs.next()
		{
			let id = state.locals.len();

			state.locals.push(Local::default());
			state.todo.push(fetch_auth(id, seed, fetch));
		}

		let (id, event_id, event) = state.todo.next().await?;

		let Global { subgraph, todo, locals } = &mut state;
		let local = &mut locals[id];

		if let Ok(event) = event {
			local.path.push(event_id);
			local
				.stack
				.push(event.auth_events_into().into_iter().collect());
		}

		let mut outputs = Path::new();
		while let Some(event_id) = local.pop() {
			if let Some(next_id) = local.eval(subgraph, event_id, &mut outputs) {
				todo.push(fetch_auth(id, next_id, fetch));
				break;
			}
		}

		if local.stack.is_empty() {
			*local = Local::default();
		}

		Some((outputs.into_iter().stream(), (inputs, state)))
	})
	.flatten()
}

fn fetch_auth<Fetch, Fut, Pdu>(
	id: usize,
	event_id: OwnedEventId,
	fetch: &Fetch,
) -> impl Future<Output = (usize, OwnedEventId, Result<Pdu>)> + Send
where
	Fetch: Fn(OwnedEventId) -> Fut,
	Fut: Future<Output = Result<Pdu>> + Send,
{
	let fut = fetch(event_id.clone());

	async move { (id, event_id, fut.await) }
}

#[implement(Local)]
fn pop(&mut self) -> Option<OwnedEventId> {
	while self.stack.last().is_some_and(Frame::is_empty) {
		self.stack.pop();
		self.path.pop();
	}

	self.marked = self.marked.min(self.path.len());
	self.stack.last_mut().and_then(Frame::pop)
}

#[implement(Local)]
#[tracing::instrument(
	name = "descent",
	level = "trace",
	skip_all,
	fields(
		s = ?subgraph
			.values()
			.fold((0_u64, 0_u64), |(a, b), v| {
				(a.saturating_add(u64::from(v.subgraph)), b.saturating_add(u64::from(v.seen)))
			}),

		%event_id,
		path = self.path.len(),
		stack = self.stack.iter().flatten().count(),
	)
)]
fn eval(
	&mut self,
	subgraph: &mut Subgraph,
	event_id: OwnedEventId,
	outputs: &mut Path,
) -> Option<OwnedEventId> {
	match subgraph.get(&event_id).copied() {
		| Some(Substate { subgraph: true, .. }) => {
			self.insert_path(subgraph, &event_id, outputs);
			None
		},
		| Some(Substate { seen: true, .. }) => None,
		| substate => {
			if substate.is_some() {
				self.insert_path(subgraph, &event_id, outputs);
			} else {
				subgraph.insert(event_id.clone(), Substate { subgraph: false, seen: true });
			}

			self.path
				.first()
				.is_some_and(is_equal_to!(&event_id))
				.is_false()
				.then_some(event_id)
		},
	}
}

#[implement(Local)]
fn insert_path(&mut self, subgraph: &mut Subgraph, event_id: &EventId, outputs: &mut Path) {
	let inserted = self.path[self.marked..]
		.iter()
		.map(AsRef::as_ref)
		.chain(once(event_id))
		.filter(|event_id| insert_path_filter(subgraph, event_id))
		.map(ToOwned::to_owned);

	outputs.extend(inserted);
	self.marked = self.path.len();
}

fn insert_path_filter(subgraph: &mut Subgraph, event_id: &EventId) -> bool {
	if let Some(state) = subgraph.get_mut(event_id) {
		let inserted = !state.subgraph;

		state.subgraph = true;
		inserted
	} else {
		subgraph.insert(event_id.to_owned(), Substate { subgraph: true, seen: false });
		true
	}
}
