use futures::TryStreamExt;
use publication::{Applied, Writes, apply, plan};
use tuwunel_core::{Result, utils::IterStream};

use crate::{
	Services,
	integrity::short_ids::rooms::{Action, Census, Rooms, census, classify, owned, pending},
};

mod publication;
#[cfg(test)]
mod tests;

#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn repair(services: &Services) -> Result<Rooms> {
	let census = census(services).await?;
	let result = pending(&census);

	if census.owner_unknown {
		return Ok(result);
	}

	let (result, restored) = census
		.candidates
		.difference(&census.held)
		.copied()
		.try_stream()
		.try_fold((result, false), async |(result, restored), candidate| {
			services.server.check_running()?;
			let Some((action, writes)) = decide(services, &census, candidate).await? else {
				return Ok((result, restored));
			};

			match apply(&services.db, &writes, action).await? {
				| Applied::Done => Ok((completed(result, candidate, action), restored)),
				| Applied::Restored => Ok((result, true)),
			}
		})
		.await?;

	Ok(Rooms { restored, ..result })
}

/// What the repair writes for one candidate, decided before it writes.
///
/// A candidate whose rows do not classify, or whose move a destination refuses,
/// gets no writes. Each decision follows the writes of the candidates before it.
#[tracing::instrument(level = "trace", skip_all)]
async fn decide(
	services: &Services,
	census: &Census,
	candidate: u64,
) -> Result<Option<(Action, Writes)>> {
	let keys = census
		.keys
		.get(&candidate)
		.map(Vec::as_slice)
		.unwrap_or_default();

	let rows = owned(services, candidate, keys).await?;

	let Some(action) = classify(services, census, candidate, &rows).await? else {
		return Ok(None);
	};

	let writes = plan(services, rows, candidate, action).await?;
	let decision = writes.map(|writes| (action, writes));

	Ok(decision)
}

fn completed(mut result: Rooms, candidate: u64, action: Action) -> Rooms {
	result.unfinished.remove(&candidate);
	match action {
		| Action::Purge => result.deleted = result.deleted.saturating_add(1),
		| Action::Move(_) => result.moved = result.moved.saturating_add(1),
	}

	result
}
