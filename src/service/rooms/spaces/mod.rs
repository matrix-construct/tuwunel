mod pagination_token;
mod tests;

use std::{collections::HashMap, sync::Arc};

use conduwuit::{debug_info, err, utils::math::usize_from_f64, Error, Result};
use futures::StreamExt;
use lru_cache::LruCache;
use ruma::{
	api::{
		client::{error::ErrorKind, space::SpaceHierarchyRoomsChunk},
		federation::{
			self,
			space::{SpaceHierarchyChildSummary, SpaceHierarchyParentSummary},
		},
	},
	events::{
		room::join_rules::{JoinRule, RoomJoinRulesEventContent},
		space::child::{HierarchySpaceChildEvent, SpaceChildEventContent},
		StateEventType,
	},
	serde::Raw,
	space::SpaceRoomJoinRule,
	OwnedRoomId, OwnedServerName, RoomId, ServerName, UserId,
};
use tokio::sync::Mutex;

pub use self::pagination_token::PaginationToken;
use crate::{rooms, sending, Dep};

pub struct CachedSpaceHierarchySummary {
	summary: SpaceHierarchyParentSummary,
}

pub enum SummaryAccessibility {
	Accessible(Box<SpaceHierarchyParentSummary>),
	Inaccessible,
}

/// Identifier used to check if rooms are accessible
///
/// None is used if you want to return the room, no matter if accessible or not
pub enum Identifier<'a> {
	UserId(&'a UserId),
	ServerName(&'a ServerName),
}

pub struct Service {
	services: Services,
	pub roomid_spacehierarchy_cache:
		Mutex<LruCache<OwnedRoomId, Option<CachedSpaceHierarchySummary>>>,
}

struct Services {
	state_accessor: Dep<rooms::state_accessor::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	state: Dep<rooms::state::Service>,
	short: Dep<rooms::short::Service>,
	event_handler: Dep<rooms::event_handler::Service>,
	timeline: Dep<rooms::timeline::Service>,
	sending: Dep<sending::Service>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let config = &args.server.config;
		let cache_size = f64::from(config.roomid_spacehierarchy_cache_capacity);
		let cache_size = cache_size * config.cache_capacity_modifier;
		Ok(Arc::new(Self {
			services: Services {
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				event_handler: args
					.depend::<rooms::event_handler::Service>("rooms::event_handler"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
				sending: args.depend::<sending::Service>("sending"),
			},
			roomid_spacehierarchy_cache: Mutex::new(LruCache::new(usize_from_f64(cache_size)?)),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Gets the summary of a space using solely local information
	pub async fn get_summary_and_children_local(
		&self,
		current_room: &OwnedRoomId,
		identifier: Identifier<'_>,
	) -> Result<Option<SummaryAccessibility>> {
		if let Some(cached) = self
			.roomid_spacehierarchy_cache
			.lock()
			.await
			.get_mut(&current_room.to_owned())
			.as_ref()
		{
			return Ok(if let Some(cached) = cached {
				if self
					.is_accessible_child(
						current_room,
						&cached.summary.join_rule,
						&identifier,
						&cached.summary.allowed_room_ids,
					)
					.await
				{
					Some(SummaryAccessibility::Accessible(Box::new(cached.summary.clone())))
				} else {
					Some(SummaryAccessibility::Inaccessible)
				}
			} else {
				None
			});
		}

		if let Some(children_pdus) = self.get_stripped_space_child_events(current_room).await? {
			let summary = self
				.get_room_summary(current_room, children_pdus, &identifier)
				.await;
			if let Ok(summary) = summary {
				self.roomid_spacehierarchy_cache.lock().await.insert(
					current_room.clone(),
					Some(CachedSpaceHierarchySummary { summary: summary.clone() }),
				);

				Ok(Some(SummaryAccessibility::Accessible(Box::new(summary))))
			} else {
				Ok(None)
			}
		} else {
			Ok(None)
		}
	}

	/// Gets the summary of a space using solely federation
	#[tracing::instrument(level = "debug", skip(self))]
	async fn get_summary_and_children_federation(
		&self,
		current_room: &OwnedRoomId,
		suggested_only: bool,
		user_id: &UserId,
		via: &[OwnedServerName],
	) -> Result<Option<SummaryAccessibility>> {
		for server in via {
			debug_info!("Asking {server} for /hierarchy");
			let Ok(response) = self
				.services
				.sending
				.send_federation_request(server, federation::space::get_hierarchy::v1::Request {
					room_id: current_room.to_owned(),
					suggested_only,
				})
				.await
			else {
				continue;
			};

			debug_info!("Got response from {server} for /hierarchy\n{response:?}");
			let summary = response.room.clone();

			self.roomid_spacehierarchy_cache.lock().await.insert(
				current_room.clone(),
				Some(CachedSpaceHierarchySummary { summary: summary.clone() }),
			);

			for child in response.children {
				let mut guard = self.roomid_spacehierarchy_cache.lock().await;
				if !guard.contains_key(current_room) {
					guard.insert(
						current_room.clone(),
						Some(CachedSpaceHierarchySummary {
							summary: {
								let SpaceHierarchyChildSummary {
									canonical_alias,
									name,
									num_joined_members,
									room_id,
									topic,
									world_readable,
									guest_can_join,
									avatar_url,
									join_rule,
									room_type,
									allowed_room_ids,
								} = child;

								SpaceHierarchyParentSummary {
									canonical_alias,
									name,
									num_joined_members,
									room_id: room_id.clone(),
									topic,
									world_readable,
									guest_can_join,
									avatar_url,
									join_rule,
									room_type,
									children_state: self
										.get_stripped_space_child_events(&room_id)
										.await?
										.unwrap(),
									allowed_room_ids,
								}
							},
						}),
					);
				}
			}
			if self
				.is_accessible_child(
					current_room,
					&response.room.join_rule,
					&Identifier::UserId(user_id),
					&response.room.allowed_room_ids,
				)
				.await
			{
				return Ok(Some(SummaryAccessibility::Accessible(Box::new(summary.clone()))));
			}

			return Ok(Some(SummaryAccessibility::Inaccessible));
		}

		self.roomid_spacehierarchy_cache
			.lock()
			.await
			.insert(current_room.clone(), None);

		Ok(None)
	}

	/// Gets the summary of a space using either local or remote (federation)
	/// sources
	pub async fn get_summary_and_children_client(
		&self,
		current_room: &OwnedRoomId,
		suggested_only: bool,
		user_id: &UserId,
		via: &[OwnedServerName],
	) -> Result<Option<SummaryAccessibility>> {
		if let Ok(Some(response)) = self
			.get_summary_and_children_local(current_room, Identifier::UserId(user_id))
			.await
		{
			Ok(Some(response))
		} else {
			self.get_summary_and_children_federation(current_room, suggested_only, user_id, via)
				.await
		}
	}

	async fn get_room_summary(
		&self,
		current_room: &OwnedRoomId,
		children_state: Vec<Raw<HierarchySpaceChildEvent>>,
		identifier: &Identifier<'_>,
	) -> Result<SpaceHierarchyParentSummary, Error> {
		let room_id: &RoomId = current_room;

		let join_rule = self
			.services
			.state_accessor
			.room_state_get_content(room_id, &StateEventType::RoomJoinRules, "")
			.await
			.map_or(JoinRule::Invite, |c: RoomJoinRulesEventContent| c.join_rule);

		let allowed_room_ids = self
			.services
			.state_accessor
			.allowed_room_ids(join_rule.clone());

		if !self
			.is_accessible_child(
				current_room,
				&join_rule.clone().into(),
				identifier,
				&allowed_room_ids,
			)
			.await
		{
			debug_info!("User is not allowed to see room {room_id}");
			// This error will be caught later
			return Err(Error::BadRequest(
				ErrorKind::forbidden(),
				"User is not allowed to see the room",
			));
		}

		Ok(SpaceHierarchyParentSummary {
			canonical_alias: self
				.services
				.state_accessor
				.get_canonical_alias(room_id)
				.await
				.ok(),
			name: self.services.state_accessor.get_name(room_id).await.ok(),
			num_joined_members: self
				.services
				.state_cache
				.room_joined_count(room_id)
				.await
				.unwrap_or(0)
				.try_into()
				.expect("user count should not be that big"),
			room_id: room_id.to_owned(),
			topic: self
				.services
				.state_accessor
				.get_room_topic(room_id)
				.await
				.ok(),
			world_readable: self
				.services
				.state_accessor
				.is_world_readable(room_id)
				.await,
			guest_can_join: self.services.state_accessor.guest_can_join(room_id).await,
			avatar_url: self
				.services
				.state_accessor
				.get_avatar(room_id)
				.await
				.into_option()
				.unwrap_or_default()
				.url,
			join_rule: join_rule.into(),
			room_type: self
				.services
				.state_accessor
				.get_room_type(room_id)
				.await
				.ok(),
			children_state,
			allowed_room_ids,
		})
	}

	/// Simply returns the stripped m.space.child events of a room
	async fn get_stripped_space_child_events(
		&self,
		room_id: &RoomId,
	) -> Result<Option<Vec<Raw<HierarchySpaceChildEvent>>>, Error> {
		let Ok(current_shortstatehash) =
			self.services.state.get_room_shortstatehash(room_id).await
		else {
			return Ok(None);
		};

		let state: HashMap<_, Arc<_>> = self
			.services
			.state_accessor
			.state_full_ids(current_shortstatehash)
			.collect()
			.await;

		let mut children_pdus = Vec::with_capacity(state.len());
		for (key, id) in state {
			let (event_type, state_key) =
				self.services.short.get_statekey_from_short(key).await?;

			if event_type != StateEventType::SpaceChild {
				continue;
			}

			let pdu =
				self.services.timeline.get_pdu(&id).await.map_err(|e| {
					err!(Database("Event {id:?} in space state not found: {e:?}"))
				})?;

			if let Ok(content) = pdu.get_content::<SpaceChildEventContent>() {
				if content.via.is_empty() {
					continue;
				}
			}

			if OwnedRoomId::try_from(state_key).is_ok() {
				children_pdus.push(pdu.to_stripped_spacechild_state_event());
			}
		}

		Ok(Some(children_pdus))
	}

	/// With the given identifier, checks if a room is accessable
	async fn is_accessible_child(
		&self,
		current_room: &OwnedRoomId,
		join_rule: &SpaceRoomJoinRule,
		identifier: &Identifier<'_>,
		allowed_room_ids: &Vec<OwnedRoomId>,
	) -> bool {
		match identifier {
			| Identifier::ServerName(server_name) => {
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
			},
			| Identifier::UserId(user_id) => {
				if self
					.services
					.state_cache
					.is_joined(user_id, current_room)
					.await || self
					.services
					.state_cache
					.is_invited(user_id, current_room)
					.await
				{
					return true;
				}
			},
		}
		match &join_rule {
			| SpaceRoomJoinRule::Public
			| SpaceRoomJoinRule::Knock
			| SpaceRoomJoinRule::KnockRestricted => true,
			| SpaceRoomJoinRule::Restricted => {
				for room in allowed_room_ids {
					match identifier {
						| Identifier::UserId(user) => {
							if self.services.state_cache.is_joined(user, room).await {
								return true;
							}
						},
						| Identifier::ServerName(server) => {
							if self.services.state_cache.server_in_room(server, room).await {
								return true;
							}
						},
					}
				}
				false
			},
			// Invite only, Private, or Custom join rule
			| _ => false,
		}
	}
}

// Here because cannot implement `From` across ruma-federation-api and
// ruma-client-api types
impl From<CachedSpaceHierarchySummary> for SpaceHierarchyRoomsChunk {
	fn from(value: CachedSpaceHierarchySummary) -> Self {
		let SpaceHierarchyParentSummary {
			canonical_alias,
			name,
			num_joined_members,
			room_id,
			topic,
			world_readable,
			guest_can_join,
			avatar_url,
			join_rule,
			room_type,
			children_state,
			..
		} = value.summary;

		Self {
			canonical_alias,
			name,
			num_joined_members,
			room_id,
			topic,
			world_readable,
			guest_can_join,
			avatar_url,
			join_rule,
			room_type,
			children_state,
		}
	}
}

/// Here because cannot implement `From` across ruma-federation-api and
/// ruma-client-api types
#[must_use]
pub fn summary_to_chunk(summary: SpaceHierarchyParentSummary) -> SpaceHierarchyRoomsChunk {
	let SpaceHierarchyParentSummary {
		canonical_alias,
		name,
		num_joined_members,
		room_id,
		topic,
		world_readable,
		guest_can_join,
		avatar_url,
		join_rule,
		room_type,
		children_state,
		..
	} = summary;

	SpaceHierarchyRoomsChunk {
		canonical_alias,
		name,
		num_joined_members,
		room_id,
		topic,
		world_readable,
		guest_can_join,
		avatar_url,
		join_rule,
		room_type,
		children_state,
	}
}

/// Returns the children of a SpaceHierarchyParentSummary, making use of the
/// children_state field
#[must_use]
pub fn get_parent_children_via(
	parent: &SpaceHierarchyParentSummary,
	suggested_only: bool,
) -> Vec<(OwnedRoomId, Vec<OwnedServerName>)> {
	parent
		.children_state
		.iter()
		.filter_map(|raw_ce| {
			raw_ce.deserialize().map_or(None, |ce| {
				if suggested_only && !ce.content.suggested {
					None
				} else {
					Some((ce.state_key, ce.content.via))
				}
			})
		})
		.collect()
}
