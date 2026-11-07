use std::collections::BTreeSet;

use futures::TryStreamExt;
use tuwunel_core::{Err, Result, err, itertools::Itertools, utils::IterStream};
use tuwunel_database::{Database, Txn};

use super::{Services, commit};
use crate::integrity::short_ids::{
	gc::{Collected, Graph, census, pending},
	state::{depth, row},
};

#[cfg(test)]
mod tests;

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
