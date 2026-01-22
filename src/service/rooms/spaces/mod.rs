mod pagination_token;
#[cfg(test)]
mod tests;

use std::{fmt::Write, sync::Arc};

use async_trait::async_trait;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, pin_mut, stream::FuturesUnordered};
use lru_cache::LruCache;
use ruma::{
	OwnedEventId, OwnedRoomId, OwnedServerName, RoomId, ServerName, UserId,
	api::{
		client::space::SpaceHierarchyRoomsChunk,
		federation::{self, space::SpaceHierarchyParentSummary},
	},
	events::{
		StateEventType,
		space::child::{HierarchySpaceChildEvent, SpaceChildEventContent},
	},
	room::{JoinRuleSummary, RoomSummary},
	serde::Raw,
};
use tokio::sync::{Mutex, MutexGuard};
use tuwunel_core::{
	Err, Error, Event, Result, implement,
	utils::{
		IterStream,
		future::{BoolExt, TryExtExt},
		math::usize_from_f64,
		stream::{BroadbandExt, ReadyExt, TryReadyExt},
	},
};

pub use self::pagination_token::PaginationToken;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	pub roomid_spacehierarchy_cache: Mutex<Cache>,
}

pub struct CachedSpaceHierarchySummary {
	summary: SpaceHierarchyParentSummary,
}

#[expect(clippy::large_enum_variant)]
pub enum SummaryAccessibility {
	Accessible(SpaceHierarchyParentSummary),
	Inaccessible,
}

/// Identifier used to check if rooms are accessible. None is used if you want
/// to return the room, no matter if accessible or not
pub enum Identifier<'a> {
	UserId(&'a UserId),
	ServerName(&'a ServerName),
}

type Cache = LruCache<OwnedRoomId, Option<CachedSpaceHierarchySummary>>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let cache_size = f64::from(config.roomid_spacehierarchy_cache_capacity);
		let cache_size = cache_size * config.cache_capacity_modifier;
		Ok(Arc::new(Self {
			services: args.services.clone(),
			roomid_spacehierarchy_cache: Mutex::new(LruCache::new(usize_from_f64(cache_size)?)),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let roomid_spacehierarchy_cache = self
			.roomid_spacehierarchy_cache
			.lock()
			.await
			.len();

		writeln!(out, "roomid_spacehierarchy_cache: {roomid_spacehierarchy_cache}")?;

		Ok(())
	}

	async fn clear_cache(&self) {
		self.roomid_spacehierarchy_cache
			.lock()
			.await
			.clear();
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

/// Gets the summary of a space using either local or remote (federation)
/// sources
#[implement(Service)]
pub async fn get_summary_and_children_client(
	&self,
	current_room: &RoomId,
	suggested_only: bool,
	user_id: &UserId,
	via: &[OwnedServerName],
) -> Result<Option<SummaryAccessibility>> {
	let identifier = Identifier::UserId(user_id);

	if let Ok(Some(response)) = self
		.get_summary_and_children_local(current_room, &identifier)
		.await
	{
		return Ok(Some(response));
	}

	self.get_summary_and_children_federation(current_room, suggested_only, user_id, via)
		.await
}

/// Gets the summary of a space using solely local information
#[implement(Service)]
pub async fn get_summary_and_children_local(
	&self,
	current_room: &RoomId,
	identifier: &Identifier<'_>,
) -> Result<Option<SummaryAccessibility>> {
	match self
		.roomid_spacehierarchy_cache
		.lock()
		.await
		.get_mut(current_room)
		.as_ref()
	{
		| None => (), // cache miss
		| Some(None) => return Ok(None),
		| Some(Some(cached)) => {
			let join_rule = &cached.summary.summary.join_rule;
			let is_accessible_child = self.is_accessible_child(
				current_room,
				join_rule,
				identifier,
				join_rule.allowed_room_ids(),
			);

			let accessibility = if is_accessible_child.await {
				SummaryAccessibility::Accessible(cached.summary.clone())
			} else {
				SummaryAccessibility::Inaccessible
			};

			return Ok(Some(accessibility));
		},
	}

	let children_pdus: Vec<_> = self
		.get_space_child_events(current_room)
		.map(Event::into_format)
		.collect()
		.await;

	let Ok(summary) = self
		.get_room_summary(current_room, children_pdus, identifier)
		.boxed()
		.await
	else {
		return Ok(None);
	};

	self.roomid_spacehierarchy_cache
		.lock()
		.await
		.insert(
			current_room.to_owned(),
			Some(CachedSpaceHierarchySummary { summary: summary.clone() }),
		);

	Ok(Some(SummaryAccessibility::Accessible(summary)))
}

/// Gets the summary of a space using solely federation
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
async fn get_summary_and_children_federation(
	&self,
	current_room: &RoomId,
	suggested_only: bool,
	user_id: &UserId,
	via: &[OwnedServerName],
) -> Result<Option<SummaryAccessibility>> {
	let request = federation::space::get_hierarchy::v1::Request {
		room_id: current_room.to_owned(),
		suggested_only,
	};

	let mut requests: FuturesUnordered<_> = via
		.iter()
		.map(|server| {
			self.services
				.federation
				.execute(server, request.clone())
		})
		.collect();

	let Some(Ok(response)) = requests.next().await else {
		self.roomid_spacehierarchy_cache
			.lock()
			.await
			.insert(current_room.to_owned(), None);

		return Ok(None);
	};

	let summary = response.room;
	self.roomid_spacehierarchy_cache
		.lock()
		.await
		.insert(
			current_room.to_owned(),
			Some(CachedSpaceHierarchySummary { summary: summary.clone() }),
		);

	response
		.children
		.into_iter()
		.stream()
		.then(|child| {
			self.roomid_spacehierarchy_cache
				.lock()
				.map(|lock| (child, lock))
		})
		.ready_filter_map(|(child, mut cache)| {
			(!cache.contains_key(current_room)).then_some((child, cache))
		})
		.for_each(|(child, cache)| self.cache_insert(cache, current_room, child))
		.await;

	let identifier = Identifier::UserId(user_id);
	let join_rule = &summary.summary.join_rule;
	let allowed_room_ids = join_rule.allowed_room_ids();

	let is_accessible_child = self
		.is_accessible_child(current_room, join_rule, &identifier, allowed_room_ids)
		.await;

	let accessibility = if is_accessible_child {
		SummaryAccessibility::Accessible(summary)
	} else {
		SummaryAccessibility::Inaccessible
	};

	Ok(Some(accessibility))
}

#[implement(Service)]
async fn get_room_summary(
	&self,
	room_id: &RoomId,
	children_state: Vec<Raw<HierarchySpaceChildEvent>>,
	identifier: &Identifier<'_>,
) -> Result<SpaceHierarchyParentSummary, Error> {
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
		return Err!(Request(Forbidden("User is not allowed to see the room")));
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

	let summary = SpaceHierarchyParentSummary {
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

	Ok(summary)
}

/// With the given identifier, checks if a room is accessible
#[implement(Service)]
async fn is_accessible_child<'a, I>(
	&self,
	current_room: &RoomId,
	join_rule: &JoinRuleSummary,
	identifier: &Identifier<'_>,
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

		// Invite only, Private, or Custom join rule
		| _ => false,
	}
}

/// Returns the children of a SpaceHierarchyParentSummary, making use of the
/// children_state field
pub fn get_parent_children_via(
	parent: &SpaceHierarchyParentSummary,
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

#[implement(Service)]
async fn cache_insert(
	&self,
	mut cache: MutexGuard<'_, Cache>,
	current_room: &RoomId,
	child: RoomSummary,
) {
	let summary = SpaceHierarchyParentSummary {
		children_state: self
			.get_space_child_events(&child.room_id)
			.map(Event::into_format)
			.collect()
			.await,

		summary: child,
	};

	cache.insert(current_room.to_owned(), Some(CachedSpaceHierarchySummary { summary }));
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
				return None;
			}

			if RoomId::parse(&state_key).is_err() {
				return None;
			}

			Some(pdu)
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

// Here because cannot implement `From` across ruma-federation-api and
// ruma-client-api types
impl From<CachedSpaceHierarchySummary> for SpaceHierarchyRoomsChunk {
	fn from(value: CachedSpaceHierarchySummary) -> Self {
		let SpaceHierarchyParentSummary { children_state, summary } = value.summary;

		Self { children_state, summary }
	}
}

/// Here because cannot implement `From` across ruma-federation-api and
/// ruma-client-api types
#[must_use]
pub fn summary_to_chunk(summary: SpaceHierarchyParentSummary) -> SpaceHierarchyRoomsChunk {
	let SpaceHierarchyParentSummary { children_state, summary } = summary;

	SpaceHierarchyRoomsChunk { children_state, summary }
}
