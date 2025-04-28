use std::{collections::HashMap, fmt::Write, iter::once, sync::Arc};

use async_trait::async_trait;
use futures::{
	FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt, future::join_all, pin_mut,
};
use ruma::{
	EventId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId, UserId,
	events::{
		AnyStrippedStateEvent, StateEventType, TimelineEventType,
		room::{create::RoomCreateEventContent, member::RoomMemberEventContent},
	},
	serde::Raw,
};
use tuwunel_core::{
	PduEvent, Result, err,
	result::FlatOk,
	state_res::{self, StateMap},
	utils::{
		IterStream, MutexMap, MutexMapGuard, ReadyExt, calculate_hash,
		stream::{BroadbandExt, TryIgnore},
	},
	warn,
};
use tuwunel_database::{Deserialized, Ignore, Interfix, Map};

use crate::{
	Dep, globals, rooms,
	rooms::{
		short::{ShortEventId, ShortStateHash},
		state_compressor::{CompressedState, parse_compressed_state_event},
	},
};

pub struct Service {
	pub mutex: RoomMutexMap,
	services: Services,
	db: Data,
}

struct Services {
	globals: Dep<globals::Service>,
	short: Dep<rooms::short::Service>,
	spaces: Dep<rooms::spaces::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
	state_compressor: Dep<rooms::state_compressor::Service>,
	timeline: Dep<rooms::timeline::Service>,
}

struct Data {
	shorteventid_shortstatehash: Arc<Map>,
	roomid_shortstatehash: Arc<Map>,
	roomid_pduleaves: Arc<Map>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
pub type RoomMutexGuard = MutexMapGuard<OwnedRoomId, ()>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			mutex: RoomMutexMap::new(),
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				spaces: args.depend::<rooms::spaces::Service>("rooms::spaces"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				state_compressor: args
					.depend::<rooms::state_compressor::Service>("rooms::state_compressor"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
			},
			db: Data {
				shorteventid_shortstatehash: args.db["shorteventid_shortstatehash"].clone(),
				roomid_shortstatehash: args.db["roomid_shortstatehash"].clone(),
				roomid_pduleaves: args.db["roomid_pduleaves"].clone(),
			},
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex = self.mutex.len();
		writeln!(out, "state_mutex: {mutex}")?;

		Ok(())
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Service {
	/// Set the room to the given statehash and update caches.
	pub async fn force_state(
		&self,
		room_id: &RoomId,
		shortstatehash: u64,
		statediffnew: Arc<CompressedState>,
		_statediffremoved: Arc<CompressedState>,
		state_lock: &RoomMutexGuard,
	) -> Result {
		let event_ids = statediffnew
			.iter()
			.stream()
			.map(|&new| parse_compressed_state_event(new).1)
			.then(|shorteventid| {
				self.services
					.short
					.get_eventid_from_short::<Box<_>>(shorteventid)
			})
			.ignore_err();

		pin_mut!(event_ids);
		while let Some(event_id) = event_ids.next().await {
			let Ok(pdu) = self.services.timeline.get_pdu(&event_id).await else {
				continue;
			};

			match pdu.kind {
				| TimelineEventType::RoomMember => {
					let Some(user_id) = pdu
						.state_key
						.as_ref()
						.map(UserId::parse)
						.flat_ok()
					else {
						continue;
					};

					let Ok(membership_event) = pdu.get_content::<RoomMemberEventContent>() else {
						continue;
					};

					self.services
						.state_cache
						.update_membership(
							room_id,
							user_id,
							membership_event,
							&pdu.sender,
							None,
							None,
							false,
						)
						.await?;
				},
				| TimelineEventType::SpaceChild => {
					self.services
						.spaces
						.roomid_spacehierarchy_cache
						.lock()
						.await
						.remove(&pdu.room_id);
				},
				| _ => continue,
			}
		}

		self.services
			.state_cache
			.update_joined_count(room_id)
			.await;

		self.set_room_state(room_id, shortstatehash, state_lock);

		Ok(())
	}

	/// Generates a new StateHash and associates it with the incoming event.
	///
	/// This adds all current state events (not including the incoming event)
	/// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
	#[tracing::instrument(skip(self, state_ids_compressed), level = "debug")]
	pub async fn set_event_state(
		&self,
		event_id: &EventId,
		room_id: &RoomId,
		state_ids_compressed: Arc<CompressedState>,
	) -> Result<ShortStateHash> {
		const KEY_LEN: usize = size_of::<ShortEventId>();
		const VAL_LEN: usize = size_of::<ShortStateHash>();

		let shorteventid = self
			.services
			.short
			.get_or_create_shorteventid(event_id)
			.await;

		let previous_shortstatehash = self.get_room_shortstatehash(room_id).await;

		let state_hash = calculate_hash(state_ids_compressed.iter().map(|s| &s[..]));

		let (shortstatehash, already_existed) = self
			.services
			.short
			.get_or_create_shortstatehash(&state_hash)
			.await;

		if !already_existed {
			let states_parents = match previous_shortstatehash {
				| Ok(p) => {
					self.services
						.state_compressor
						.load_shortstatehash_info(p)
						.await?
				},
				| _ => Vec::new(),
			};

			let (statediffnew, statediffremoved) =
				if let Some(parent_stateinfo) = states_parents.last() {
					let statediffnew: CompressedState = state_ids_compressed
						.difference(&parent_stateinfo.full_state)
						.copied()
						.collect();

					let statediffremoved: CompressedState = parent_stateinfo
						.full_state
						.difference(&state_ids_compressed)
						.copied()
						.collect();

					(Arc::new(statediffnew), Arc::new(statediffremoved))
				} else {
					(state_ids_compressed, Arc::new(CompressedState::new()))
				};
			self.services
				.state_compressor
				.save_state_from_diff(
					shortstatehash,
					statediffnew,
					statediffremoved,
					1_000_000, // high number because no state will be based on this one
					states_parents,
				)?;
		}

		self.db
			.shorteventid_shortstatehash
			.aput::<KEY_LEN, VAL_LEN, _, _>(shorteventid, shortstatehash);

		Ok(shortstatehash)
	}

	/// Generates a new StateHash and associates it with the incoming event.
	///
	/// This adds all current state events (not including the incoming event)
	/// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
	#[tracing::instrument(skip(self, new_pdu), level = "debug")]
	pub async fn append_to_state(&self, new_pdu: &PduEvent) -> Result<u64> {
		const BUFSIZE: usize = size_of::<u64>();

		let shorteventid = self
			.services
			.short
			.get_or_create_shorteventid(&new_pdu.event_id)
			.await;

		let previous_shortstatehash = self
			.get_room_shortstatehash(&new_pdu.room_id)
			.await;

		if let Ok(p) = previous_shortstatehash {
			self.db
				.shorteventid_shortstatehash
				.aput::<BUFSIZE, BUFSIZE, _, _>(shorteventid, p);
		}

		match &new_pdu.state_key {
			| Some(state_key) => {
				let states_parents = match previous_shortstatehash {
					| Ok(p) => {
						self.services
							.state_compressor
							.load_shortstatehash_info(p)
							.await?
					},
					| _ => Vec::new(),
				};

				let shortstatekey = self
					.services
					.short
					.get_or_create_shortstatekey(&new_pdu.kind.to_string().into(), state_key)
					.await;

				let new = self
					.services
					.state_compressor
					.compress_state_event(shortstatekey, &new_pdu.event_id)
					.await;

				let replaces = states_parents
					.last()
					.map(|info| {
						info.full_state
							.iter()
							.find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
					})
					.unwrap_or_default();

				if Some(&new) == replaces {
					return Ok(previous_shortstatehash.expect("must exist"));
				}

				// TODO: statehash with deterministic inputs
				let shortstatehash = self.services.globals.next_count()?;

				let mut statediffnew = CompressedState::new();
				statediffnew.insert(new);

				let mut statediffremoved = CompressedState::new();
				if let Some(replaces) = replaces {
					statediffremoved.insert(*replaces);
				}

				self.services
					.state_compressor
					.save_state_from_diff(
						shortstatehash,
						Arc::new(statediffnew),
						Arc::new(statediffremoved),
						2,
						states_parents,
					)?;

				Ok(shortstatehash)
			},
			| _ => {
				Ok(previous_shortstatehash.expect("first event in room must be a state event"))
			},
		}
	}

	#[tracing::instrument(skip_all, level = "debug")]
	pub async fn summary_stripped(&self, event: &PduEvent) -> Vec<Raw<AnyStrippedStateEvent>> {
		let cells = [
			(&StateEventType::RoomCreate, ""),
			(&StateEventType::RoomJoinRules, ""),
			(&StateEventType::RoomCanonicalAlias, ""),
			(&StateEventType::RoomName, ""),
			(&StateEventType::RoomAvatar, ""),
			(&StateEventType::RoomMember, event.sender.as_str()), // Add recommended events
			(&StateEventType::RoomEncryption, ""),
			(&StateEventType::RoomTopic, ""),
		];

		let fetches = cells.iter().map(|(event_type, state_key)| {
			self.services
				.state_accessor
				.room_state_get(&event.room_id, event_type, state_key)
		});

		join_all(fetches)
			.await
			.into_iter()
			.filter_map(Result::ok)
			.map(PduEvent::into_stripped_state_event)
			.chain(once(event.to_stripped_state_event()))
			.collect()
	}

	/// Set the state hash to a new version, but does not update state_cache.
	#[tracing::instrument(skip(self, _mutex_lock), level = "debug")]
	pub fn set_room_state(
		&self,
		room_id: &RoomId,
		shortstatehash: u64,
		_mutex_lock: &RoomMutexGuard, /* Take mutex guard to make sure users get the room
		                               * state mutex */
	) {
		const BUFSIZE: usize = size_of::<u64>();

		self.db
			.roomid_shortstatehash
			.raw_aput::<BUFSIZE, _, _>(room_id, shortstatehash);
	}

	/// Returns the room's version.
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn get_room_version(&self, room_id: &RoomId) -> Result<RoomVersionId> {
		self.services
			.state_accessor
			.room_state_get_content(room_id, &StateEventType::RoomCreate, "")
			.await
			.map(|content: RoomCreateEventContent| content.room_version)
			.map_err(|e| err!(Request(NotFound("No create event found: {e:?}"))))
	}

	pub async fn get_room_shortstatehash(&self, room_id: &RoomId) -> Result<ShortStateHash> {
		self.db
			.roomid_shortstatehash
			.get(room_id)
			.await
			.deserialized()
	}

	pub fn get_forward_extremities<'a>(
		&'a self,
		room_id: &'a RoomId,
	) -> impl Stream<Item = &EventId> + Send + '_ {
		let prefix = (room_id, Interfix);

		self.db
			.roomid_pduleaves
			.keys_prefix(&prefix)
			.map_ok(|(_, event_id): (Ignore, &EventId)| event_id)
			.ignore_err()
	}

	pub async fn set_forward_extremities<'a, I>(
		&'a self,
		room_id: &'a RoomId,
		event_ids: I,
		_state_lock: &'a RoomMutexGuard,
	) where
		I: Iterator<Item = &'a EventId> + Send + 'a,
	{
		let prefix = (room_id, Interfix);
		self.db
			.roomid_pduleaves
			.keys_prefix_raw(&prefix)
			.ignore_err()
			.ready_for_each(|key| self.db.roomid_pduleaves.remove(key))
			.await;

		for event_id in event_ids {
			let key = (room_id, event_id);
			self.db.roomid_pduleaves.put_raw(key, event_id);
		}
	}

	/// This fetches auth events from the current state.
	#[tracing::instrument(skip(self, content), level = "debug")]
	pub async fn get_auth_events(
		&self,
		room_id: &RoomId,
		kind: &TimelineEventType,
		sender: &UserId,
		state_key: Option<&str>,
		content: &serde_json::value::RawValue,
	) -> Result<StateMap<PduEvent>> {
		let Ok(shortstatehash) = self.get_room_shortstatehash(room_id).await else {
			return Ok(HashMap::new());
		};

		let auth_types = state_res::auth_types_for_event(kind, sender, state_key, content)?;

		let sauthevents: HashMap<_, _> = auth_types
			.iter()
			.stream()
			.broad_filter_map(|(event_type, state_key)| {
				self.services
					.short
					.get_shortstatekey(event_type, state_key)
					.map_ok(move |ssk| (ssk, (event_type, state_key)))
					.map(Result::ok)
			})
			.collect()
			.await;

		let (state_keys, event_ids): (Vec<_>, Vec<_>) = self
			.services
			.state_accessor
			.state_full_shortids(shortstatehash)
			.ready_filter_map(Result::ok)
			.ready_filter_map(|(shortstatekey, shorteventid)| {
				sauthevents
					.get(&shortstatekey)
					.map(|(ty, sk)| ((ty, sk), shorteventid))
			})
			.unzip()
			.await;

		self.services
			.short
			.multi_get_eventid_from_short(event_ids.into_iter().stream())
			.zip(state_keys.into_iter().stream())
			.ready_filter_map(|(event_id, (ty, sk))| Some(((ty, sk), event_id.ok()?)))
			.broad_filter_map(|((ty, sk), event_id): (_, OwnedEventId)| async move {
				self.services
					.timeline
					.get_pdu(&event_id)
					.await
					.map(move |pdu| (((*ty).clone(), (*sk).clone()), pdu))
					.ok()
			})
			.collect()
			.map(Ok)
			.await
	}
}
