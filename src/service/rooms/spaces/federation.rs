use futures::{StreamExt, pin_mut, stream::FuturesUnordered};
use ruma::{
	OwnedServerName, RoomId,
	api::federation::space::{
		SpaceHierarchyParentSummary as ParentSummary,
		get_hierarchy::v1::{Request, Response},
	},
};
use tuwunel_core::{
	Err, Event, Result, debug, implement,
	utils::{
		stream::{BroadbandExt, IterStream, ReadyExt},
		timepoint_has_passed,
	},
};

use super::{
	Accessibility,
	Accessibility::{Accessible, Inaccessible},
	Identifier,
};

/// Gets the summary of a space using solely federation
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

	let requests: FuturesUnordered<_> = via
		.iter()
		.map(|server| {
			self.services
				.federation
				.execute(server, request.clone())
		})
		.collect();

	pin_mut!(requests);
	debug!(?current_room, ?sender, requests = requests.len(), "requesting...");
	let Some(Ok(Response { room, children, .. })) = requests.next().await else {
		self.cache_put(current_room, None);
		return Err!(Request(NotFound("Space room not found over federation.")));
	};

	self.cache_put(current_room, Some(room.clone()));

	children
		.into_iter()
		.stream()
		.broad_filter_map(async |summary| {
			self.cache_get(&summary.room_id)
				.await
				.ok()
				.map(|cached| cached.expires)
				.is_none_or(timepoint_has_passed)
				.then_some(ParentSummary {
					children_state: self
						.get_space_child_events(&summary.room_id)
						.map(Event::into_format)
						.collect()
						.await,

					summary,
				})
		})
		.map(|summary| (summary.summary.room_id.clone(), summary))
		.ready_for_each(|(room_id, summary)| {
			self.cache_put(&room_id, Some(summary));
		})
		.await;

	self.is_accessible_child(
		current_room,
		&room.summary.join_rule,
		sender,
		room.summary.join_rule.allowed_room_ids(),
	)
	.await
	.then(|| Ok(Accessible(room)))
	.unwrap_or(Ok(Inaccessible))
}
