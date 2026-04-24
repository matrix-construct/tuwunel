use futures::{StreamExt, pin_mut, stream::FuturesUnordered};
use ruma::{
	OwnedServerName, RoomId,
	api::federation::space::{
		SpaceHierarchyParentSummary as ParentSummary,
		get_hierarchy::v1::{Request, Response},
	},
	room::RoomType,
};
use tuwunel_core::{Err, Result, debug, implement, trace};

use super::{
	Accessibility,
	Accessibility::{Accessible, Inaccessible},
	Identifier,
};

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

	let requests: FuturesUnordered<_> = via
		.iter()
		.map(|server| {
			self.services
				.federation
				.execute(server, request.clone())
		})
		.collect();

	pin_mut!(requests);
	debug!(
		?current_room,
		?sender,
		?via,
		requests = requests.len(),
		"waiting for federation response"
	);

	let Some(Ok(Response { room, children, inaccessible_children })) = requests.next().await
	else {
		self.cache_put(current_room, None);
		return Err!(Request(NotFound("Space room not found over federation.")));
	};

	trace!(
		?current_room,
		?sender,
		?room,
		?children,
		?inaccessible_children,
		"federation response"
	);

	for room_id in &inaccessible_children {
		self.cache_put(room_id, None);
	}

	for summary in children
		.into_iter()
		.filter(|child| child.room_type.ne(&Some(RoomType::Space)))
	{
		let room_id = summary.room_id.clone();
		let summary = ParentSummary {
			summary,
			children_state: Default::default(),
		};

		self.cache_put(&room_id, Some(&summary));
	}

	self.cache_put(current_room, Some(&room));

	self.is_accessible_child(current_room, &room.summary.join_rule.clone(), sender)
		.await
		.then(|| Ok(Accessible(room)))
		.unwrap_or(Ok(Inaccessible))
}
