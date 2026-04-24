mod cache;
mod federation;
mod local;
mod pagination_token;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use async_trait::async_trait;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, pin_mut};
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedServerName, RoomId, ServerName, UserId,
	api::{
		client::space::SpaceHierarchyRoomsChunk,
		federation::space::SpaceHierarchyParentSummary as ParentSummary,
	},
	events::{
		StateEventType,
		space::child::{HierarchySpaceChildEvent as ChildEvent, SpaceChildEventContent},
	},
	room::JoinRuleSummary,
	serde::Raw,
};
use tuwunel_core::{
	Err, Event, Result, implement,
	utils::{
		future::{BoolExt, TryExtExt},
		stream::{BroadbandExt, IterStream, ReadyExt, TryReadyExt},
	},
};
use tuwunel_database::Map;

use self::cache::Cached;
pub use self::pagination_token::PaginationToken;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Db,
}

struct Db {
	roomid_spacehierarchy: Arc<Map>,
}

#[expect(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Accessibility {
	Accessible(ParentSummary),
	Inaccessible,
}

/// Identifier used to check if rooms are accessible. None is used if you want
/// to return the room, no matter if accessible or not
#[derive(Debug)]
pub enum Identifier<'a> {
	UserId(&'a UserId),
	ServerName(&'a ServerName),
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			db: Db {
				roomid_spacehierarchy: args.db["roomid_spacehierarchy"].clone(),
			},
		}))
	}

	async fn clear_cache(&self) { self.db.roomid_spacehierarchy.clear().await; }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Gets the summary of a space using either local or remote (federation)
/// sources
#[implement(Service)]
#[tracing::instrument(
	name = "summary",
	level = "debug",
	ret(level = "trace")
	skip_all,
	fields(
		?room_id,
		?sender,
		via = via.len(),
	)
)]
pub async fn get_summary_and_children(
	&self,
	room_id: &RoomId,
	sender: &Identifier<'_>,
	via: &[OwnedServerName],
) -> Result<Accessibility> {
	debug_assert!(
		matches!(sender, Identifier::UserId(_)) || via.is_empty(),
		"The federation handler must not produce federation requests.",
	);

	self.get_summary_and_children_local(room_id, sender)
		.or_else(async |e| match e {
			| _ if !e.is_not_found() => Err(e),

			| _ if via.is_empty() =>
				Err!(Request(NotFound("Space room not found locally; not querying federation"))),

			| _ =>
				self.get_summary_and_children_federation(room_id, sender, via)
					.boxed()
					.await,
		})
		.await
}

/// Simply returns the stripped m.space.child events of a room
#[implement(Service)]
pub fn get_space_children<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = OwnedRoomId> + Send + 'a {
	self.services
		.state_accessor
		.room_state_keys(room_id, &StateEventType::SpaceChild)
		.ready_and_then(|state_key| OwnedRoomId::parse(state_key.as_str()).map_err(Into::into))
		.ready_filter_map(Result::ok)
}

/// Simply returns the stripped m.space.child events of a room
#[implement(Service)]
fn get_space_child_events<'a>(
	&'a self,
	room_id: &'a RoomId,
) -> impl Stream<Item = impl Event> + Send + 'a {
	self.services
		.state_accessor
		.room_state_keys_with_ids(room_id, &StateEventType::SpaceChild)
		.ready_filter_map(Result::ok)
		.broad_filter_map(async |(state_key, event_id): (_, OwnedEventId)| {
			self.services
				.timeline
				.get_pdu(&event_id)
				.map_ok(move |pdu| (state_key, pdu))
				.ok()
				.await
		})
		.ready_filter_map(|(state_key, pdu)| {
			let Ok(content) = pdu.get_content::<SpaceChildEventContent>() else {
				return None;
			};

			if content.via.is_empty() {
				return None;
			}

			if RoomId::parse(&state_key).is_err() {
				return None;
			}

			Some(pdu)
		})
}

/// With the given identifier, checks if a room is accessible
#[implement(Service)]
async fn is_accessible_child<'a, I>(
	&self,
	current_room: &RoomId,
	join_rule: &JoinRuleSummary,
	sender: &Identifier<'_>,
	allowed_rooms: I,
) -> bool
where
	I: Iterator<Item = &'a RoomId> + Send,
{
	if let Identifier::ServerName(server_name) = sender {
		// Checks if ACLs allow for the server to participate
		if self
			.services
			.event_handler
			.acl_check(server_name, current_room)
			.await
			.is_err()
		{
			return false;
		}
	}

	if let Identifier::UserId(user_id) = sender {
		let is_joined = self
			.services
			.state_cache
			.is_joined(user_id, current_room);

		let is_invited = self
			.services
			.state_cache
			.is_invited(user_id, current_room);

		pin_mut!(is_joined, is_invited);
		if is_joined.or(is_invited).await {
			return true;
		}
	}

	match *join_rule {
		| JoinRuleSummary::Public
		| JoinRuleSummary::Knock
		| JoinRuleSummary::KnockRestricted(_) => true,
		| JoinRuleSummary::Restricted(_) =>
			allowed_rooms
				.stream()
				.any(async |room| match sender {
					| Identifier::UserId(user) =>
						self.services
							.state_cache
							.is_joined(user, room)
							.await,

					| Identifier::ServerName(server) =>
						self.services
							.state_cache
							.server_in_room(server, room)
							.await,
				})
				.await,

		| _ => false, // Invite only, Private, or Custom join rule
	}
}

/// Returns the children of a SpaceHierarchyParentSummary, making use of the
/// children_state field
pub fn get_parent_children_via(
	parent: &ParentSummary,
	suggested_only: bool,
) -> impl DoubleEndedIterator<Item = (OwnedRoomId, impl Iterator<Item = OwnedServerName>)> + '_ {
	parent
		.children_state
		.iter()
		.map(Raw::deserialize)
		.filter_map(Result::ok)
		.filter_map(move |ChildEvent { state_key, content, .. }: _| {
			(content.suggested || !suggested_only).then_some((state_key, content.via.into_iter()))
		})
}

/// Here because cannot implement `From` across ruma-federation-api and
/// ruma-client-api types
#[inline]
#[must_use]
pub fn summary_to_chunk(
	ParentSummary { children_state, summary }: ParentSummary,
) -> SpaceHierarchyRoomsChunk {
	SpaceHierarchyRoomsChunk { children_state, summary }
}

#[inline]
#[must_use]
pub fn is_summary_serializable(summary: &ParentSummary) -> bool {
	// Ignore case to workaround a Ruma issue which refuses to serialize unknown
	// join rule types.
	!matches!(summary.summary.join_rule, JoinRuleSummary::_Custom(_))
}
