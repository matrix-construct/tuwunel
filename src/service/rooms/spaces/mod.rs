mod pagination_token;
#[cfg(test)]
mod tests;

use std::{sync::Arc, time::SystemTime};

use async_trait::async_trait;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, pin_mut, stream::FuturesUnordered};
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedServerName, RoomId, ServerName, UserId,
	api::{
		client::space::SpaceHierarchyRoomsChunk,
		federation::space::SpaceHierarchyParentSummary as ParentSummary,
	},
	events::{
		StateEventType,
		space::child::{HierarchySpaceChildEvent, SpaceChildEventContent},
	},
	room::{JoinRuleSummary, RoomSummary},
	serde::Raw,
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Err, Error, Event, Result, at, debug, error, implement,
	utils::{
		future::{BoolExt, TryExtExt},
		rand::time_from_now_secs,
		stream::{BroadbandExt, IterStream, ReadyExt, TryReadyExt},
		timepoint_has_passed,
	},
};
use tuwunel_database::{Deserialized, Json, Map};

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	db: Db,
}

struct Db {
	roomid_spacehierarchy: Arc<Map>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Cached {
	expires: SystemTime,
	summary: Option<ParentSummary>,
}

#[allow(clippy::large_enum_variant)]
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

pub use self::pagination_token::PaginationToken;

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

#[implement(Service)]
#[inline]
#[tracing::instrument(name = "evict", level = "debug", skip(self))]
pub fn cache_evict(&self, room_id: &RoomId) { self.db.roomid_spacehierarchy.remove(room_id); }

/// Gets the summary of a space using either local or remote (federation)
/// sources
#[implement(Service)]
#[tracing::instrument(
	name = "summary",
	level = "debug",
	skip_all,
	fields(
		?room_id,
		?user_id,
		via = via.len(),
	)
)]
pub async fn get_summary_and_children_client(
	&self,
	room_id: &RoomId,
	user_id: &UserId,
	via: &[OwnedServerName],
) -> Result<Accessibility> {
	self.get_summary_and_children_local(room_id, Identifier::UserId(user_id))
		.or_else(async |e| match e {
			| _ if !e.is_not_found() => Err(e),
			| _ if via.is_empty() =>
				Err!(Request(NotFound("No servers provided for federation request."))),
			| _ =>
				self.get_summary_and_children_federation(room_id, user_id, via)
					.boxed()
					.await,
		})
		.await
}

/// Gets the summary of a space using solely local information
#[implement(Service)]
#[tracing::instrument(name = "local", level = "debug", skip_all)]
pub async fn get_summary_and_children_local(
	&self,
	current_room: &RoomId,
	identifier: Identifier<'_>,
) -> Result<Accessibility> {
	use Accessibility::{Accessible, Inaccessible};

	match self
		.db
		.roomid_spacehierarchy
		.get(current_room)
		.await
		.deserialized::<Json<_>>()
		.map(at!(0))
	{
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
					identifier,
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
		.get_room_summary(current_room, children_state, identifier)
		.boxed()
		.await?;

	if let Accessible(summary) = &summary {
		self.cache_put(current_room, Some(summary.clone()));
	}

	Ok(summary)
}

/// Gets the summary of a space using solely federation
#[implement(Service)]
#[tracing::instrument(name = "federation", level = "debug", skip(self))]
async fn get_summary_and_children_federation(
	&self,
	current_room: &RoomId,
	user_id: &UserId,
	via: &[OwnedServerName],
) -> Result<Accessibility> {
	use Accessibility::{Accessible, Inaccessible};
	use ruma::api::federation::space::get_hierarchy::v1::{Request, Response};

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
	debug!(?current_room, ?user_id, requests = requests.len(), "requesting...");
	let Some(Ok(Response { room, children, .. })) = requests.next().await else {
		self.cache_put(current_room, None);
		return Err!(Request(NotFound("Space room not found over federation.")));
	};

	self.cache_put(current_room, Some(room.clone()));

	children
		.into_iter()
		.stream()
		.filter_map(async |child| {
			self.db
				.roomid_spacehierarchy
				.get(&child.room_id)
				.await
				.deserialized::<Json<Cached>>()
				.ok()
				.map(at!(0))
				.map(|cached| cached.expires)
				.is_none_or(timepoint_has_passed)
				.then_some(child)
		})
		.broad_then(async |child| ParentSummary {
			children_state: self
				.get_space_child_events(&child.room_id)
				.map(Event::into_format)
				.collect()
				.await,

			summary: child,
		})
		.for_each(async |summary| {
			let room_id = summary.summary.room_id.clone();
			self.cache_put(&room_id, Some(summary));
		})
		.await;

	self.is_accessible_child(
		current_room,
		&room.summary.join_rule,
		Identifier::UserId(user_id),
		room.summary.join_rule.allowed_room_ids(),
	)
	.await
	.then(|| Ok(Accessible(room)))
	.unwrap_or(Ok(Inaccessible))
}

/// Returns the children of a SpaceHierarchyParentSummary, making use of the
/// children_state field
pub fn get_parent_children_via(
	parent: &ParentSummary,
	suggested_only: bool,
) -> impl DoubleEndedIterator<
	Item = (OwnedRoomId, impl Iterator<Item = OwnedServerName> + Send + use<>),
> + '_ {
	parent
		.children_state
		.iter()
		.map(Raw::deserialize)
		.filter_map(Result::ok)
		.filter_map(move |ce| {
			(!suggested_only || ce.content.suggested)
				.then_some((ce.state_key, ce.content.via.into_iter()))
		})
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
			if let Ok(content) = pdu.get_content::<SpaceChildEventContent>()
				&& content.via.is_empty()
			{
				//return None;
			}

			if RoomId::parse(&state_key).is_err() {
				return None;
			}

			Some(pdu)
		})
}

#[implement(Service)]
async fn get_room_summary(
	&self,
	room_id: &RoomId,
	children_state: Vec<Raw<HierarchySpaceChildEvent>>,
	identifier: Identifier<'_>,
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
			identifier,
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

/// With the given identifier, checks if a room is accessible
#[implement(Service)]
async fn is_accessible_child<'a, I>(
	&self,
	current_room: &RoomId,
	join_rule: &JoinRuleSummary,
	identifier: Identifier<'_>,
	allowed_rooms: I,
) -> bool
where
	I: Iterator<Item = &'a RoomId> + Send,
{
	if let Identifier::ServerName(server_name) = identifier {
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

	if let Identifier::UserId(user_id) = identifier {
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
				.any(async |room| match identifier {
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

#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip(self, summary),
	fields(summary = summary.is_some())
)]
fn cache_put(&self, room_id: &RoomId, summary: Option<ParentSummary>) {
	debug!("cache put");
	self.db.roomid_spacehierarchy.raw_put(
		room_id,
		Json(Cached {
			expires: self.generate_ttl(),
			summary: summary.filter(is_summary_serializable),
		}),
	);
}

#[implement(Service)]
fn generate_ttl(&self) -> SystemTime {
	time_from_now_secs(
		self.services.config.spacehierarchy_cache_ttl_min
			..self.services.config.spacehierarchy_cache_ttl_max,
	)
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

/// Here because cannot implement `From` across ruma-federation-api and
/// ruma-client-api types
impl From<Cached> for Option<SpaceHierarchyRoomsChunk> {
	#[inline]
	fn from(value: Cached) -> Self {
		value
			.summary
			.map(|ParentSummary { children_state, summary }: ParentSummary| {
				SpaceHierarchyRoomsChunk { children_state, summary }
			})
	}
}

#[inline]
#[must_use]
pub fn is_summary_serializable(summary: &ParentSummary) -> bool {
	// Ignore case to workaround a Ruma issue which refuses to serialize unknown
	// join rule types.
	!matches!(summary.summary.join_rule, JoinRuleSummary::_Custom(_))
}
