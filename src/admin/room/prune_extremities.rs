use std::borrow::Borrow;

use futures::StreamExt;
use ruma::OwnedRoomOrAliasId;
use tuwunel_core::Result;
use tuwunel_service::rooms::state::{PruneSummary, Trigger};

use crate::admin_command;

#[admin_command]
#[tracing::instrument(level = "debug", skip(self))]
pub(super) async fn room_prune_extremities(
	&self,
	room_id: OwnedRoomOrAliasId,
	target: Option<usize>,
	dry_run: bool,
) -> Result {
	let room_id = self
		.services
		.alias
		.maybe_resolve(&room_id)
		.await?;

	let state_lock = self.services.state.mutex.lock(&room_id).await;

	let mut band = self
		.services
		.state
		.get_forward_extremities(&room_id)
		.map(ToOwned::to_owned)
		.collect::<Vec<_>>()
		.await;

	let config = &self.services.server.config;
	let target = target
		.unwrap_or(config.forward_extremities_max)
		.max(1);

	let goal = band.len().saturating_sub(target);

	let summary = self
		.services
		.state
		.prune_forward_extremities(&room_id, &mut band, goal, Trigger::Admin)
		.await;

	if !dry_run {
		self.services
			.state
			.set_forward_extremities(&room_id, band.iter().map(Borrow::borrow), &state_lock)
			.await;
	}

	let verb = if dry_run { "Would prune" } else { "Pruned" };
	let PruneSummary {
		before,
		after,
		dangling,
		referenced,
		message,
		state,
	} = summary;

	let out = format!(
		"{verb} {room_id}: {before} to {after} forward extremities (swept {dangling} dangling, \
		 {referenced} referenced; dropped {message} message, {state} state)."
	);

	self.write_str(&out).await
}
