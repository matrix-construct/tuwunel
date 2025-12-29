use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	RoomId, api::federation::space::SpaceHierarchyParentSummary as ParentSummary,
	events::space::child::HierarchySpaceChildEvent, room::RoomSummary, serde::Raw,
};
use tuwunel_core::{
	Err, Error, Event, Result, debug, error, implement,
	utils::{future::TryExtExt, timepoint_has_passed},
};

use super::{Accessibility, Cached, Identifier};

/// Gets the summary of a space using solely local information
#[implement(super::Service)]
#[tracing::instrument(name = "local", level = "debug", skip_all)]
pub(super) async fn get_summary_and_children_local(
	&self,
	current_room: &RoomId,
	sender: &Identifier<'_>,
) -> Result<Accessibility> {
	use Accessibility::{Accessible, Inaccessible};

	match self.cache_get(current_room).await {
		| Err(e) if !e.is_not_found() => {
			error!(?current_room, "cache error: {e}");
			return Err(e);
		},
		| Err(_) => {
			debug!(?current_room, "cache miss");
			return Err!(Request(NotFound("Space room not found.")));
		},
		| Ok(Cached { expires, .. }) if timepoint_has_passed(expires) => {
			debug!(?current_room, ?expires, "cache expired");
			return Err!(Request(NotFound("Space room not found. (cached; expired)")));
		},
		| Ok(Cached { expires, summary: Some(cached) }) => {
			debug!(?current_room, ?expires, "cache hit");
			return self
				.is_accessible_child(
					current_room,
					&cached.summary.join_rule,
					sender,
					cached.summary.join_rule.allowed_room_ids(),
				)
				.await
				.then(|| Ok(Accessible(cached)))
				.unwrap_or(Ok(Inaccessible));
		},
		| Ok(Cached { expires, summary: None, .. }) => {
			debug!(?current_room, ?expires, "cache negative");
		},
	}

	let children_state: Vec<_> = self
		.get_space_child_events(current_room)
		.map(Event::into_format)
		.collect()
		.await;

	let summary = self
		.get_room_summary(current_room, children_state, sender)
		.boxed()
		.await?;

	if let Accessible(summary) = &summary {
		self.cache_put(current_room, Some(summary.clone()));
	}

	Ok(summary)
}

#[implement(super::Service)]
pub(super) async fn get_room_summary(
	&self,
	room_id: &RoomId,
	children_state: Vec<Raw<HierarchySpaceChildEvent>>,
	sender: &Identifier<'_>,
) -> Result<Accessibility, Error> {
	let join_rule = self
		.services
		.state_accessor
		.get_join_rules(room_id)
		.await;

	let is_accessible_child = self
		.is_accessible_child(
			room_id,
			&join_rule.clone().into(),
			sender,
			join_rule.allowed_room_ids(),
		)
		.await;

	if !is_accessible_child {
		return Ok(Accessibility::Inaccessible);
	}

	let name = self
		.services
		.state_accessor
		.get_name(room_id)
		.ok();

	let topic = self
		.services
		.state_accessor
		.get_room_topic(room_id)
		.ok();

	let room_type = self
		.services
		.state_accessor
		.get_room_type(room_id)
		.ok();

	let world_readable = self
		.services
		.state_accessor
		.is_world_readable(room_id);

	let guest_can_join = self
		.services
		.state_accessor
		.guest_can_join(room_id);

	let num_joined_members = self
		.services
		.state_cache
		.room_joined_count(room_id)
		.unwrap_or(0);

	let canonical_alias = self
		.services
		.state_accessor
		.get_canonical_alias(room_id)
		.ok();

	let avatar_url = self
		.services
		.state_accessor
		.get_avatar(room_id)
		.map_ok(|content| content.url)
		.ok();

	let room_version = self.services.state.get_room_version(room_id).ok();

	let encryption = self
		.services
		.state_accessor
		.get_room_encryption(room_id)
		.ok();

	let (
		canonical_alias,
		name,
		num_joined_members,
		topic,
		world_readable,
		guest_can_join,
		avatar_url,
		room_type,
		room_version,
		encryption,
	) = futures::join!(
		canonical_alias,
		name,
		num_joined_members,
		topic,
		world_readable,
		guest_can_join,
		avatar_url,
		room_type,
		room_version,
		encryption,
	);

	let summary = ParentSummary {
		children_state,
		summary: RoomSummary {
			avatar_url: avatar_url.flatten(),
			canonical_alias,
			name,
			topic,
			world_readable,
			guest_can_join,
			room_type,
			encryption,
			room_version,
			room_id: room_id.to_owned(),
			num_joined_members: num_joined_members.try_into().unwrap_or_default(),
			join_rule: join_rule.clone().into(),
		},
	};

	Ok(Accessibility::Accessible(summary))
}
