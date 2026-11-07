use std::collections::BTreeSet;

use futures::TryStreamExt;
use tuwunel_core::{Result, utils::IterStream};

use super::{
	short_of,
	state::{Parents, ancestors, decode, depth},
	sweep,
};
use crate::Services;

pub(crate) struct Collected {
	pub(crate) deleted: u64,
	pub(crate) unfinished: BTreeSet<u64>,
	pub(crate) unknown: bool,
}

#[derive(Default)]
pub(crate) struct Graph {
	pub(crate) parents: Parents,
	held: BTreeSet<u64>,
	pub(crate) unknown: bool,
	malformed: bool,
}

pub(crate) const ROOTS: [&str; 4] = [
	"roomid_shortstatehash",
	"shorteventid_shortstatehash",
	"eventid_resolvedstate",
	"statehash_shortstatehash",
];

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn inspect(services: &Services) -> Result<Collected> {
	Ok(pending(&census(services).await?))
}

#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn census(services: &Services) -> Result<Graph> {
	let graph = sweep(services, "shortstatehash_statediff", Graph::default(), index).await?;

	ROOTS
		.try_stream()
		.try_fold(graph, async |graph, column| {
			sweep(services, column, graph, |graph, _, value| root(graph, value)).await
		})
		.await
}

fn index(graph: Graph, key: &[u8], value: &[u8]) -> Graph {
	let id = short_of(key).filter(|id| *id != 0);
	let parent = value.get(..8).and_then(short_of);
	let malformed = id.is_none() || decode(value).is_none();

	record(graph, id, parent, malformed)
}

fn record(mut graph: Graph, id: Option<u64>, parent: Option<u64>, malformed: bool) -> Graph {
	graph.unknown |= parent.is_none();
	let parent = parent.filter(|id| *id != 0);

	if let Some(id) = id {
		graph.parents.insert(id, parent);
	}

	if malformed {
		graph.malformed = true;
		graph.held.extend(id.into_iter().chain(parent));
	}

	graph
}

fn root(mut graph: Graph, value: &[u8]) -> Graph {
	graph.malformed |= value.len() != 8;

	match value.get(..8).and_then(short_of) {
		| None | Some(0) => graph.unknown = true,
		| Some(id) => {
			graph.held.insert(id);
		},
	}

	graph
}

pub(crate) fn pending(graph: &Graph) -> Collected {
	let broken: Vec<_> = graph
		.parents
		.keys()
		.copied()
		.filter(|id| depth(&graph.parents, *id).is_none())
		.collect();

	let retained = retained(graph, &broken);
	let unfinished = graph
		.parents
		.keys()
		.copied()
		.filter(|id| !retained.contains(id))
		.collect();

	Collected {
		deleted: 0,
		unfinished,
		unknown: graph.unknown || graph.malformed || !broken.is_empty(),
	}
}

fn retained(graph: &Graph, broken: &[u64]) -> BTreeSet<u64> {
	graph
		.held
		.iter()
		.chain(broken)
		.flat_map(|id| ancestors(&graph.parents, *id))
		.collect()
}
