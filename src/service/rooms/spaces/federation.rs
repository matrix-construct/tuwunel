use futures::StreamExt;
use ruma::{
	OwnedServerName, RoomId,
	api::federation::space::{
		SpaceHierarchyParentSummary as ParentSummary,
		get_hierarchy::v1::{Request, Response},
	},
	room::RoomType,
};
use tuwunel_core::{Err, Result, debug, implement, utils::IterStream};

use super::{
	Accessibility,
	Accessibility::{Accessible, Inaccessible},
	Identifier,
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
		.first_acceptable(|_| true)
		.await
		.map(|(_, response)| response);

	let Some(Response { room, children, inaccessible_children }) = response else {
		self.cache_put(current_room, None);
		return Err!(Request(NotFound("Space room not found over federation.")));
	};

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
