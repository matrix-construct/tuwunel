use std::collections::BTreeSet;

use futures::TryStreamExt;
use tuwunel_core::{Err, Result, err, itertools::Itertools, utils::IterStream};
use tuwunel_database::{Database, Txn};

use super::{super::sweep, Parents, Services, ancestors, commit, decode, depth, row, short_of};

#[cfg(test)]
mod tests;

pub(in super::super) struct Collected {
	pub(in super::super) deleted: u64,
	pub(in super::super) unfinished: BTreeSet<u64>,
	pub(in super::super) unknown: bool,
}

#[derive(Default)]
struct Graph {
	parents: Parents,
	held: BTreeSet<u64>,
	unknown: bool,
	malformed: bool,
}

const ROOTS: [&str; 4] = [
	"roomid_shortstatehash",
	"shorteventid_shortstatehash",
	"eventid_resolvedstate",
	"statehash_shortstatehash",
];

#[tracing::instrument(level = "debug", skip_all)]
pub(in super::super) async fn collect(services: &Services) -> Result<Collected> {
	let graph = census(services).await?;
	let pending = pending(&graph);

	if graph.unknown {
		return Ok(pending);
	}

	remove(services, &graph, pending).await
}

#[tracing::instrument(level = "debug", skip_all)]
pub(in super::super) async fn inspect(services: &Services) -> Result<Collected> {
	Ok(pending(&census(services).await?))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn census(services: &Services) -> Result<Graph> {
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

fn pending(graph: &Graph) -> Collected {
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

#[tracing::instrument(level = "debug", skip_all)]
async fn remove(services: &Services, graph: &Graph, pending: Collected) -> Result<Collected> {
	let (deletable, unfinished): (Vec<_>, BTreeSet<_>) = pending
		.unfinished
		.into_iter()
		.map(|id| {
			depth(&graph.parents, id)
				.map(|depth| (depth, id))
				.ok_or(id)
		})
		.partition_result();

	let deleted = deletable
		.into_iter()
		.sorted_unstable()
		.rev()
		.try_stream()
		.try_fold(pending.deleted, async |deleted, (_, id)| {
			services.server.check_running()?;
			commit(services, deletion(&services.db, id)).map_err(|error| err!("{error}"))?;

			if row(&services.db, id).await?.is_some() {
				return Err!("state deletion verification failed");
			}

			Ok(deleted.saturating_add(1))
		})
		.await?;

	Ok(Collected {
		deleted,
		unfinished,
		unknown: pending.unknown,
	})
}

fn deletion(db: &Database, id: u64) -> Txn {
	let mut txn = db.txn();

	txn.del_raw(&db["shortstatehash_statediff"], id.to_be_bytes());

	txn
}
