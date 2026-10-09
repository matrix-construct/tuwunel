use futures::{StreamExt, TryStreamExt};
use ruma::{
	OwnedServerName, RoomId,
	api::federation::space::{
		SpaceHierarchyParentSummary as ParentSummary,
		get_hierarchy::v1::{Request, Response},
	},
	room::RoomType,
};
use tuwunel_core::{
	Err, Error, Result, debug, implement,
	utils::{IterStream, stream::WidebandExt},
};

use super::{
	Accessibility,
	Accessibility::{Accessible, Inaccessible},
	Identifier,
	cache::Provenance,
};
use crate::federation::feds::{Fault, Opts, OutcomeExt, Record};

/// Gets the summary of a space using solely federation.
#[implement(super::Service)]
#[tracing::instrument(
	name = "federation",
	level = "debug",
	err(level = "debug"),
	ret(level = "trace"),
	skip(self)
)]
pub(super) async fn get_summary_and_children_federation(
	&self,
	current_room: &RoomId,
	sender: &Identifier<'_>,
	via: &[OwnedServerName],
) -> Result<Accessibility> {
	let request = Request {
		room_id: current_room.to_owned(),
		suggested_only: false,
	};

	debug!(
		?current_room,
		?sender,
		?via,
		requests = via.len(),
		"waiting for federation response"
	);
	let opts = Opts {
		record: Record::Contribute,
		..Default::default()
	};

	let response = self
		.services
		.federation
		.fanout_to(via.iter().cloned().stream(), move |_| request.clone(), opts)
		.inspect(|outcome| match &outcome.result {
			| Ok(response) => debug!(?response, "federation response"),
			| Err(Fault::Error(error)) => debug!(?error, "federation error"),
			| Err(fault) => debug!(?fault, "federation error"),
		})
		.first_acceptable(|response| summary_matches_room(&response.room, current_room))
		.await
		.map(|(_, response)| response);

	let Some(Response { room, children, inaccessible_children }) = response else {
		if self
			.services
			.state_cache
			.server_in_room_result(self.services.server.name.as_ref(), current_room)
			.await?
		{
			return self
				.get_summary_and_children_local(current_room, sender)
				.await;
		}

		self.cache_put(current_room, None, Provenance::Remote);
		return Err!(Request(NotFound("Space room not found over federation.")));
	};

	if self
		.services
		.state_cache
		.server_in_room_result(self.services.server.name.as_ref(), current_room)
		.await?
	{
		return self
			.get_summary_and_children_local(current_room, sender)
			.await;
	}

	let accessible = self
		.is_accessible_child(current_room, &room.summary.join_rule, sender)
		.await;

	let inaccessible_children: Vec<_> = inaccessible_children
		.into_iter()
		.stream()
		.wide_then(async |room_id| {
			Ok::<_, Error>(
				self.remote_room(&room_id)
					.await?
					.then_some(room_id),
			)
		})
		.try_collect()
		.await?;

	let children: Vec<_> = children
		.into_iter()
		.filter(|child| child.room_type.ne(&Some(RoomType::Space)))
		.stream()
		.wide_then(async |summary| {
			Ok::<_, Error>(
				self.remote_room(&summary.room_id)
					.await?
					.then_some(summary),
			)
		})
		.try_collect()
		.await?;

	if self
		.services
		.state_cache
		.server_in_room_result(self.services.server.name.as_ref(), current_room)
		.await?
	{
		return self
			.get_summary_and_children_local(current_room, sender)
			.await;
	}

	inaccessible_children
		.into_iter()
		.flatten()
		.for_each(|room_id| self.cache_put(&room_id, None, Provenance::Remote));

	children
		.into_iter()
		.flatten()
		.for_each(|summary| {
			let summary = ParentSummary {
				summary,
				children_state: Default::default(),
			};

			self.cache_put(&summary.summary.room_id, Some(&summary), Provenance::Remote);
		});

	self.cache_put(current_room, Some(&room), Provenance::Remote);

	accessible
		.then(|| Ok(Accessible(room)))
		.unwrap_or(Ok(Inaccessible))
}

#[implement(super::Service)]
#[tracing::instrument(level = "trace", skip(self))]
pub(super) async fn remote_room(&self, room_id: &RoomId) -> Result<bool> {
	let remote = !self
		.services
		.state_cache
		.server_in_room_result(self.services.server.name.as_ref(), room_id)
		.await?;

	#[cfg(test)]
	let remote = super::tests::after_remote_room(remote).await;

	Ok(remote)
}

#[inline]
pub(super) fn summary_matches_room(summary: &ParentSummary, room_id: &RoomId) -> bool {
	summary.summary.room_id == room_id
}
