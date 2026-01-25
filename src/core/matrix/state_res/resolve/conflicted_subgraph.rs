use std::{
	collections::HashSet as Set,
	mem::take,
	sync::{Arc, Mutex},
};

use futures::{Future, FutureExt, Stream, StreamExt};
use ruma::OwnedEventId;

use crate::{
	Result, debug,
	matrix::Event,
	utils::stream::{IterStream, automatic_width},
};

#[derive(Default)]
struct Global {
	subgraph: Mutex<Set<OwnedEventId>>,
	seen: Mutex<Set<OwnedEventId>>,
}

#[derive(Default)]
struct Local {
	path: Vec<OwnedEventId>,
	stack: Vec<Vec<OwnedEventId>>,
}

#[tracing::instrument(name = "conflicted_subgraph", level = "debug", skip_all)]
pub(super) fn conflicted_subgraph_dfs<ConflictedEventIds, Fetch, Fut, Pdu>(
	conflicted_event_ids: ConflictedEventIds,
	fetch: &Fetch,
) -> impl Stream<Item = OwnedEventId> + Send
where
	ConflictedEventIds: Stream<Item = OwnedEventId> + Send,
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	conflicted_event_ids
		.collect::<Set<_>>()
		.map(|ids| (Arc::new(Global::default()), ids))
		.then(async |(state, conflicted_event_ids)| {
			conflicted_event_ids
				.iter()
				.stream()
				.map(ToOwned::to_owned)
				.map(|event_id| (state.clone(), event_id))
				.for_each_concurrent(automatic_width(), async |(state, event_id)| {
					subgraph_descent(state, event_id, &conflicted_event_ids, fetch)
						.await
						.expect("only mutex errors expected");
				})
				.await;

			let mut state = state.subgraph.lock().expect("locked");
			debug!(
				input_event = conflicted_event_ids.len(),
				output_events = state.len(),
				"conflicted subgraph state"
			);

			take(&mut *state)
		})
		.map(Set::into_iter)
		.map(IterStream::stream)
		.flatten_stream()
}

#[tracing::instrument(
	name = "descent",
	level = "trace",
	skip_all,
	fields(
		event_id = %conflicted_event_id,
		event_ids = conflicted_event_ids.len(),
	)
)]
async fn subgraph_descent<Fetch, Fut, Pdu>(
	state: Arc<Global>,
	conflicted_event_id: OwnedEventId,
	conflicted_event_ids: &Set<OwnedEventId>,
	fetch: &Fetch,
) -> Result<Arc<Global>>
where
	Fetch: Fn(OwnedEventId) -> Fut + Sync,
	Fut: Future<Output = Result<Pdu>> + Send,
	Pdu: Event,
{
	let Global { subgraph, seen } = &*state;

	let mut local = Local {
		path: vec![conflicted_event_id.clone()],
		stack: vec![vec![conflicted_event_id]],
	};

	while let Some(event_id) = pop(&mut local) {
		if subgraph.lock()?.contains(&event_id) {
			if local.path.len() > 1 {
				subgraph
					.lock()?
					.extend(local.path.iter().cloned());
			}

			local.path.pop();
			continue;
		}

		if !seen.lock()?.insert(event_id.clone()) {
			continue;
		}

		if local.path.len() > 1 && conflicted_event_ids.contains(&event_id) {
			subgraph
				.lock()?
				.extend(local.path.iter().cloned());
		}

		if let Ok(event) = fetch(event_id).await {
			local
				.stack
				.push(event.auth_events_into().into_iter().collect());
		}
	}

	Ok(state)
}

fn pop(local: &mut Local) -> Option<OwnedEventId> {
	let Local { path, stack } = local;

	while stack.last().is_some_and(Vec::is_empty) {
		stack.pop();
		path.pop();
	}

	stack
		.last_mut()
		.and_then(Vec::pop)
		.inspect(|event_id| path.push(event_id.clone()))
}
