use std::{
	collections::{HashMap, VecDeque},
	iter::once,
	ops::Deref,
};

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
	waiters: Waiters,
	ready: Ready,
	deferred: Deferred,
	parked: usize,
}

struct Context<'a> {
	subgraph: &'a mut Subgraph,
	waiters: &'a mut Waiters,
	ready: &'a mut Ready,
	parked: &'a mut usize,
	outputs: &'a mut Path,
}

#[derive(Debug, Default)]
struct Local {
	path: Path,
	stack: Stack,
	marked: usize,
}

#[derive(Debug)]
struct Wake {
	event_id: OwnedEventId,
	locals: Waiting,
	result: Resolution,
}

#[derive(Debug)]
enum Evaluation {
	Continue,
	Fetch(OwnedEventId),
	Park,
}

#[derive(Clone, Copy, Debug)]
enum Resolution {
	Dead,
	Subgraph,
}

#[derive(Clone, Copy, Debug)]
enum Substate {
	Conflicted,
	Pending,
	Dead,
	Subgraph,
}

type Todo<Fut> = FuturesUnordered<Fut>;
type Subgraph = HashMap<OwnedEventId, Substate, RandomState>;
type Locals = Vec<Local>;
type Waiters = HashMap<OwnedEventId, Waiting, RandomState>;
type Waiting = SmallVec<[usize; WAITING_INLINE]>;
type Ready = Vec<Wake>;
type Deferred = VecDeque<(usize, OwnedEventId)>;
type Path = SmallVec<[OwnedEventId; PATH_INLINE]>;
type Stack = SmallVec<[Frame; STACK_INLINE]>;
type Frame = AuthEvents;

const PATH_INLINE: usize = 4;
const STACK_INLINE: usize = 4;
const WAITING_INLINE: usize = 1;
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

	subgraph.extend(seeds().map(|event_id| (event_id, Substate::Conflicted)));

	let state = Global {
		subgraph,
		todo: Todo::new(),
		locals: Locals::with_capacity(conflicted_set.len()),
		waiters: Waiters::with_hasher(RandomState),
		ready: Ready::new(),
		deferred: Deferred::new(),
		parked: 0,
	};

	unfold((seeds(), state), async |(mut inputs, mut state)| {
		let width = automatic_width();

		debug_assert!(
			state.todo.len() <= width,
			"Excessive in-flight conflicted-subgraph fetches"
		);

		while state.todo.len() < width {
			if let Some((id, event_id)) = state.deferred.pop_front() {
				state.todo.push(fetch_auth(id, event_id, fetch));
				continue;
			}

			let Some(seed) = inputs.next() else {
				break;
			};

			let id = state.locals.len();

			state.locals.push(Local::default());
			state.todo.push(fetch_auth(id, seed, fetch));
		}

		let Some((id, event_id, event)) = state.todo.next().await else {
			debug_assert!(state.waiters.is_empty(), "Unresolved conflicted-subgraph waiters");
			debug_assert!(state.ready.is_empty(), "Undrained conflicted-subgraph wakes");
			debug_assert!(state.deferred.is_empty(), "Deferred conflicted-subgraph fetches");
			debug_assert_eq!(state.parked, 0, "Parked conflicted-subgraph walkers");
			return None;
		};

		while state.todo.len() < width
			&& let Some((deferred_id, deferred_event_id)) = state.deferred.pop_front()
		{
			state
				.todo
				.push(fetch_auth(deferred_id, deferred_event_id, fetch));
		}

		let mut outputs = Path::new();

		if let Some(next_id) = process_fetch(&mut state, id, event_id, event, &mut outputs) {
			if state.todo.len() < width {
				state.todo.push(fetch_auth(id, next_id, fetch));
			} else {
				state.deferred.push_back((id, next_id));
			}
		}

		while let Some(Wake { event_id, locals, result }) = state.ready.pop() {
			for id in locals {
				if let Some(next_id) = resume(&mut state, id, &event_id, result, &mut outputs) {
					if state.todo.len() < width {
						state.todo.push(fetch_auth(id, next_id, fetch));
					} else {
						state.deferred.push_back((id, next_id));
					}
				}
			}
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

fn process_fetch<Fut, Pdu>(
	state: &mut Global<Fut>,
	id: usize,
	event_id: OwnedEventId,
	event: Result<Pdu>,
	outputs: &mut Path,
) -> Option<OwnedEventId>
where
	Fut: Future + Send,
	Pdu: Event,
{
	match event {
		| Ok(event) => {
			let local = &mut state.locals[id];

			local.path.push(event_id);
			local
				.stack
				.push(event.auth_events_into().into_iter().collect());
		},
		| Err(_) => {
			let Global { subgraph, waiters, ready, parked, .. } = state;
			let mut context = Context {
				subgraph,
				waiters,
				ready,
				parked,
				outputs,
			};

			complete_pending(&mut context, event_id, Resolution::Dead);
		},
	}

	advance(state, id, outputs)
}

fn resume<Fut: Future + Send>(
	state: &mut Global<Fut>,
	id: usize,
	event_id: &EventId,
	result: Resolution,
	outputs: &mut Path,
) -> Option<OwnedEventId> {
	if matches!(result, Resolution::Subgraph) {
		let Global {
			subgraph, locals, waiters, ready, parked, ..
		} = state;

		let mut context = Context {
			subgraph,
			waiters,
			ready,
			parked,
			outputs,
		};

		locals[id].insert_path(&mut context, event_id);
	}

	advance(state, id, outputs)
}

fn advance<Fut: Future + Send>(
	state: &mut Global<Fut>,
	id: usize,
	outputs: &mut Path,
) -> Option<OwnedEventId> {
	let Global {
		subgraph, locals, waiters, ready, parked, ..
	} = state;

	let local = &mut locals[id];
	let mut context = Context {
		subgraph,
		waiters,
		ready,
		parked,
		outputs,
	};

	while let Some(event_id) = local.pop(&mut context) {
		match local.eval(id, &mut context, event_id) {
			| Evaluation::Continue => {},
			| Evaluation::Fetch(event_id) => return Some(event_id),
			| Evaluation::Park => return None,
		}
	}

	if local.stack.is_empty() {
		*local = Local::default();
	}

	None
}

#[implement(Local)]
fn pop(&mut self, context: &mut Context<'_>) -> Option<OwnedEventId> {
	while self.stack.last().is_some_and(Frame::is_empty) {
		self.stack.pop();

		if let Some(event_id) = self.path.pop() {
			complete_pending(context, event_id, Resolution::Dead);
		}
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
		s = ?context
			.subgraph
			.values()
			.fold((0_u64, 0_u64, 0_u64, 0_u64), |(pending, dead, conflicted, subgraph), state| {
				match state {
					| Substate::Pending => (pending.saturating_add(1), dead, conflicted, subgraph),
					| Substate::Dead => (pending, dead.saturating_add(1), conflicted, subgraph),
					| Substate::Conflicted => {
						(pending, dead, conflicted.saturating_add(1), subgraph)
					},
					| Substate::Subgraph => {
						(pending, dead, conflicted, subgraph.saturating_add(1))
					},
				}
			}),

		%event_id,
		path = self.path.len(),
		stack = self.stack.iter().flatten().count(),
	)
)]
fn eval(&mut self, id: usize, context: &mut Context<'_>, event_id: OwnedEventId) -> Evaluation {
	match context.subgraph.get(&event_id).copied() {
		| Some(Substate::Subgraph) => {
			self.insert_path(context, &event_id);
			Evaluation::Continue
		},
		| Some(Substate::Dead) => Evaluation::Continue,
		| Some(Substate::Pending) => {
			if self.path.iter().any(is_equal_to!(&event_id)) {
				return Evaluation::Continue;
			}

			context
				.waiters
				.entry(event_id)
				.or_default()
				.push(id);

			*context.parked = context.parked.saturating_add(1);
			Evaluation::Park
		},
		| Some(Substate::Conflicted) => {
			self.insert_path(context, &event_id);

			self.path
				.first()
				.is_some_and(is_equal_to!(&event_id))
				.is_false()
				.then_some(event_id)
				.map_or(Evaluation::Continue, Evaluation::Fetch)
		},
		| None => {
			context
				.subgraph
				.insert(event_id.clone(), Substate::Pending);

			Evaluation::Fetch(event_id)
		},
	}
}

#[implement(Local)]
fn insert_path(&mut self, context: &mut Context<'_>, event_id: &EventId) {
	let Context {
		subgraph,
		waiters,
		ready,
		parked,
		outputs,
	} = context;

	let inserted = self.path[self.marked..]
		.iter()
		.map(AsRef::as_ref)
		.chain(once(event_id))
		.filter(|event_id| insert_path_filter(subgraph, waiters, ready, parked, event_id))
		.map(ToOwned::to_owned);

	outputs.extend(inserted);
	self.marked = self.path.len();
}

fn insert_path_filter(
	subgraph: &mut Subgraph,
	waiters: &mut Waiters,
	ready: &mut Ready,
	parked: &mut usize,
	event_id: &EventId,
) -> bool {
	let Some(state) = subgraph.get_mut(event_id) else {
		subgraph.insert(event_id.to_owned(), Substate::Subgraph);
		return true;
	};

	if matches!(*state, Substate::Subgraph) {
		return false;
	}

	let pending = matches!(*state, Substate::Pending);

	debug_assert!(
		!matches!(*state, Substate::Dead),
		"Dead node inserted into conflicted subgraph"
	);

	*state = Substate::Subgraph;

	if pending
		&& !waiters.is_empty()
		&& let Some(locals) = waiters.remove(event_id)
	{
		debug_assert!(*parked >= locals.len(), "Invalid parked walker count");
		*parked = parked.saturating_sub(locals.len());
		ready.push(Wake {
			event_id: event_id.to_owned(),
			locals,
			result: Resolution::Subgraph,
		});
	}

	true
}

fn complete_pending(context: &mut Context<'_>, event_id: OwnedEventId, result: Resolution) {
	let Some(state) = context.subgraph.get_mut(&event_id) else {
		return;
	};

	if !matches!(*state, Substate::Pending) {
		return;
	}

	*state = match result {
		| Resolution::Dead => Substate::Dead,
		| Resolution::Subgraph => Substate::Subgraph,
	};

	if context.waiters.is_empty() {
		return;
	}

	let Some(locals) = context.waiters.remove(&event_id) else {
		return;
	};

	debug_assert!(*context.parked >= locals.len(), "Invalid parked walker count");
	*context.parked = context.parked.saturating_sub(locals.len());
	context
		.ready
		.push(Wake { event_id, locals, result });
}
