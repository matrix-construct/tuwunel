mod data;

use std::{
	borrow::Borrow,
	cmp,
	collections::{BTreeMap, HashSet},
	fmt::Write,
	iter::once,
	sync::Arc,
};

use async_trait::async_trait;
use futures::{
	Future, FutureExt, Stream, StreamExt, TryStreamExt, future, future::ready, pin_mut,
};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedRoomId, OwnedServerName,
	RoomId, RoomVersionId, ServerName, UserId,
	api::federation,
	canonical_json::to_canonical_value,
	events::{
		GlobalAccountDataEventType, StateEventType, TimelineEventType,
		push_rules::PushRulesEvent,
		room::{
			create::RoomCreateEventContent,
			encrypted::Relation,
			member::{MembershipState, RoomMemberEventContent},
			power_levels::RoomPowerLevelsEventContent,
			redaction::RoomRedactionEventContent,
		},
	},
	push::{Action, Ruleset, Tweak},
	uint,
};
use serde::Deserialize;
use serde_json::value::{RawValue as RawJsonValue, to_raw_value};
pub use tuwunel_core::matrix::pdu::{PduId, RawPduId};
use tuwunel_core::{
	Err, Error, Result, Server, at, debug, debug_warn, err, error, implement, info,
	matrix::{
		Event,
		pdu::{EventHash, PduBuilder, PduCount, PduEvent, gen_event_id},
		state_res::{self, RoomVersion},
	},
	utils::{
		self, IterStream, MutexMap, MutexMapGuard, ReadyExt, future::TryExtExt, stream::TryIgnore,
	},
	validated, warn,
};

use self::data::Data;
pub use self::data::PdusIterItem;
use crate::{
	Dep, account_data, admin, appservice,
	appservice::NamespaceRegex,
	globals, pusher, rooms,
	rooms::{short::ShortRoomId, state_compressor::CompressedState},
	sending, server_keys, users,
};

// Update Relationships
#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: Relation,
}

#[derive(Clone, Debug, Deserialize)]
struct ExtractEventId {
	event_id: OwnedEventId,
}
#[derive(Clone, Debug, Deserialize)]
struct ExtractRelatesToEventId {
	#[serde(rename = "m.relates_to")]
	relates_to: ExtractEventId,
}

#[derive(Deserialize)]
struct ExtractBody {
	body: Option<String>,
}

pub struct Service {
	services: Services,
	db: Data,
	pub mutex_insert: RoomMutexMap,
}

struct Services {
	server: Arc<Server>,
	account_data: Dep<account_data::Service>,
	appservice: Dep<appservice::Service>,
	admin: Dep<admin::Service>,
	alias: Dep<rooms::alias::Service>,
	globals: Dep<globals::Service>,
	short: Dep<rooms::short::Service>,
	state: Dep<rooms::state::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
	pdu_metadata: Dep<rooms::pdu_metadata::Service>,
	read_receipt: Dep<rooms::read_receipt::Service>,
	sending: Dep<sending::Service>,
	server_keys: Dep<server_keys::Service>,
	user: Dep<rooms::user::Service>,
	users: Dep<users::Service>,
	pusher: Dep<pusher::Service>,
	threads: Dep<rooms::threads::Service>,
	search: Dep<rooms::search::Service>,
	spaces: Dep<rooms::spaces::Service>,
	event_handler: Dep<rooms::event_handler::Service>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
pub type RoomMutexGuard = MutexMapGuard<OwnedRoomId, ()>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: Services {
				server: args.server.clone(),
				account_data: args.depend::<account_data::Service>("account_data"),
				appservice: args.depend::<appservice::Service>("appservice"),
				admin: args.depend::<admin::Service>("admin"),
				alias: args.depend::<rooms::alias::Service>("rooms::alias"),
				globals: args.depend::<globals::Service>("globals"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				pdu_metadata: args.depend::<rooms::pdu_metadata::Service>("rooms::pdu_metadata"),
				read_receipt: args.depend::<rooms::read_receipt::Service>("rooms::read_receipt"),
				sending: args.depend::<sending::Service>("sending"),
				server_keys: args.depend::<server_keys::Service>("server_keys"),
				user: args.depend::<rooms::user::Service>("rooms::user"),
				users: args.depend::<users::Service>("users"),
				pusher: args.depend::<pusher::Service>("pusher"),
				threads: args.depend::<rooms::threads::Service>("rooms::threads"),
				search: args.depend::<rooms::search::Service>("rooms::search"),
				spaces: args.depend::<rooms::spaces::Service>("rooms::spaces"),
				event_handler: args
					.depend::<rooms::event_handler::Service>("rooms::event_handler"),
			},
			db: Data::new(&args),
			mutex_insert: RoomMutexMap::new(),
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_insert = self.mutex_insert.len();
		writeln!(out, "insert_mutex: {mutex_insert}")?;

		Ok(())
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Service {
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn first_pdu_in_room(&self, room_id: &RoomId) -> Result<PduEvent> {
		self.first_item_in_room(room_id).await.map(at!(1))
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn first_item_in_room(&self, room_id: &RoomId) -> Result<(PduCount, PduEvent)> {
		let pdus = self.pdus(None, room_id, None);

		pin_mut!(pdus);
		pdus.try_next()
			.await?
			.ok_or_else(|| err!(Request(NotFound("No PDU found in room"))))
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn latest_pdu_in_room(&self, room_id: &RoomId) -> Result<PduEvent> {
		self.db.latest_pdu_in_room(None, room_id).await
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn last_timeline_count(
		&self,
		sender_user: Option<&UserId>,
		room_id: &RoomId,
	) -> Result<PduCount> {
		self.db
			.last_timeline_count(sender_user, room_id)
			.await
	}

	/// Returns the `count` of this pdu's id.
	pub async fn get_pdu_count(&self, event_id: &EventId) -> Result<PduCount> {
		self.db.get_pdu_count(event_id).await
	}

	/// Returns the json of a pdu.
	pub async fn get_pdu_json(&self, event_id: &EventId) -> Result<CanonicalJsonObject> {
		self.db.get_pdu_json(event_id).await
	}

	/// Returns the json of a pdu.
	#[inline]
	pub async fn get_non_outlier_pdu_json(
		&self,
		event_id: &EventId,
	) -> Result<CanonicalJsonObject> {
		self.db.get_non_outlier_pdu_json(event_id).await
	}

	/// Returns the pdu's id.
	#[inline]
	pub async fn get_pdu_id(&self, event_id: &EventId) -> Result<RawPduId> {
		self.db.get_pdu_id(event_id).await
	}

	/// Returns the pdu.
	///
	/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
	#[inline]
	pub async fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
		self.db.get_non_outlier_pdu(event_id).await
	}

	/// Returns the pdu.
	///
	/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
	pub async fn get_pdu(&self, event_id: &EventId) -> Result<PduEvent> {
		self.db.get_pdu(event_id).await
	}

	/// Returns the pdu.
	///
	/// This does __NOT__ check the outliers `Tree`.
	pub async fn get_pdu_from_id(&self, pdu_id: &RawPduId) -> Result<PduEvent> {
		self.db.get_pdu_from_id(pdu_id).await
	}

	/// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
	pub async fn get_pdu_json_from_id(&self, pdu_id: &RawPduId) -> Result<CanonicalJsonObject> {
		self.db.get_pdu_json_from_id(pdu_id).await
	}

	/// Checks if pdu exists
	///
	/// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
	pub fn pdu_exists<'a>(
		&'a self,
		event_id: &'a EventId,
	) -> impl Future<Output = bool> + Send + 'a {
		self.db.pdu_exists(event_id).is_ok()
	}

	/// Removes a pdu and creates a new one with the same id.
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn replace_pdu(
		&self,
		pdu_id: &RawPduId,
		pdu_json: &CanonicalJsonObject,
		pdu: &PduEvent,
	) -> Result<()> {
		self.db.replace_pdu(pdu_id, pdu_json, pdu).await
	}

	/// Creates a new persisted data unit and adds it to a room.
	///
	/// By this point the incoming event should be fully authenticated, no auth
	/// happens in `append_pdu`.
	///
	/// Returns pdu id
	#[tracing::instrument(level = "debug", skip_all)]
	pub async fn append_pdu<'a, Leafs>(
		&'a self,
		pdu: &'a PduEvent,
		mut pdu_json: CanonicalJsonObject,
		leafs: Leafs,
		state_lock: &'a RoomMutexGuard,
	) -> Result<RawPduId>
	where
		Leafs: Iterator<Item = &'a EventId> + Send + 'a,
	{
		// Coalesce database writes for the remainder of this scope.
		let _cork = self.db.db.cork_and_flush();

		let shortroomid = self
			.services
			.short
			.get_shortroomid(&pdu.room_id)
			.await
			.map_err(|_| err!(Database("Room does not exist")))?;

		// Make unsigned fields correct. This is not properly documented in the spec,
		// but state events need to have previous content in the unsigned field, so
		// clients can easily interpret things like membership changes
		if let Some(state_key) = &pdu.state_key {
			if let CanonicalJsonValue::Object(unsigned) = pdu_json
				.entry("unsigned".to_owned())
				.or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::default()))
			{
				if let Ok(shortstatehash) = self
					.services
					.state_accessor
					.pdu_shortstatehash(&pdu.event_id)
					.await
				{
					if let Ok(prev_state) = self
						.services
						.state_accessor
						.state_get(shortstatehash, &pdu.kind.to_string().into(), state_key)
						.await
					{
						unsigned.insert(
							"prev_content".to_owned(),
							CanonicalJsonValue::Object(
								utils::to_canonical_object(prev_state.content.clone()).map_err(
									|e| {
										error!(
											"Failed to convert prev_state to canonical JSON: {e}"
										);
										Error::bad_database(
											"Failed to convert prev_state to canonical JSON.",
										)
									},
								)?,
							),
						);
						unsigned.insert(
							String::from("prev_sender"),
							CanonicalJsonValue::String(prev_state.sender.to_string()),
						);
						unsigned.insert(
							String::from("replaces_state"),
							CanonicalJsonValue::String(prev_state.event_id.to_string()),
						);
					}
				}
			} else {
				error!("Invalid unsigned type in pdu.");
			}
		}

		// We must keep track of all events that have been referenced.
		self.services
			.pdu_metadata
			.mark_as_referenced(&pdu.room_id, pdu.prev_events.iter().map(AsRef::as_ref));

		self.services
			.state
			.set_forward_extremities(&pdu.room_id, leafs, state_lock)
			.await;

		let insert_lock = self.mutex_insert.lock(&pdu.room_id).await;

		let count1 = self.services.globals.next_count().unwrap();
		// Mark as read first so the sending client doesn't get a notification even if
		// appending fails
		self.services
			.read_receipt
			.private_read_set(&pdu.room_id, &pdu.sender, count1);
		self.services
			.user
			.reset_notification_counts(&pdu.sender, &pdu.room_id);

		let count2 = PduCount::Normal(self.services.globals.next_count().unwrap());
		let pdu_id: RawPduId = PduId { shortroomid, shorteventid: count2 }.into();

		// Insert pdu
		self.db
			.append_pdu(&pdu_id, pdu, &pdu_json, count2)
			.await;

		drop(insert_lock);

		// See if the event matches any known pushers via power level
		let power_levels: RoomPowerLevelsEventContent = self
			.services
			.state_accessor
			.room_state_get_content(&pdu.room_id, &StateEventType::RoomPowerLevels, "")
			.await
			.unwrap_or_default();

		let sync_pdu = pdu.to_sync_room_event();

		let mut push_target: HashSet<_> = self
			.services
			.state_cache
			.active_local_users_in_room(&pdu.room_id)
			.map(ToOwned::to_owned)
			// Don't notify the sender of their own events, and dont send from ignored users
			.ready_filter(|user| *user != pdu.sender)
			.filter_map(|recipient_user| async move { (!self.services.users.user_is_ignored(&pdu.sender, &recipient_user).await).then_some(recipient_user) })
			.collect()
			.await;

		let mut notifies = Vec::with_capacity(push_target.len().saturating_add(1));
		let mut highlights = Vec::with_capacity(push_target.len().saturating_add(1));

		if pdu.kind == TimelineEventType::RoomMember {
			if let Some(state_key) = &pdu.state_key {
				let target_user_id = UserId::parse(state_key)?;

				if self
					.services
					.users
					.is_active_local(target_user_id)
					.await
				{
					push_target.insert(target_user_id.to_owned());
				}
			}
		}

		for user in &push_target {
			let rules_for_user = self
				.services
				.account_data
				.get_global(user, GlobalAccountDataEventType::PushRules)
				.await
				.map_or_else(
					|_| Ruleset::server_default(user),
					|ev: PushRulesEvent| ev.content.global,
				);

			let mut highlight = false;
			let mut notify = false;

			for action in self
				.services
				.pusher
				.get_actions(user, &rules_for_user, &power_levels, &sync_pdu, &pdu.room_id)
				.await
			{
				match action {
					| Action::Notify => notify = true,
					| Action::SetTweak(Tweak::Highlight(true)) => {
						highlight = true;
					},
					| _ => {},
				}

				// Break early if both conditions are true
				if notify && highlight {
					break;
				}
			}

			if notify {
				notifies.push(user.clone());
			}

			if highlight {
				highlights.push(user.clone());
			}

			self.services
				.pusher
				.get_pushkeys(user)
				.ready_for_each(|push_key| {
					self.services
						.sending
						.send_pdu_push(&pdu_id, user, push_key.to_owned())
						.expect("TODO: replace with future");
				})
				.await;
		}

		self.db
			.increment_notification_counts(&pdu.room_id, notifies, highlights);

		match pdu.kind {
			| TimelineEventType::RoomRedaction => {
				use RoomVersionId::*;

				let room_version_id = self
					.services
					.state
					.get_room_version(&pdu.room_id)
					.await?;
				match room_version_id {
					| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
						if let Some(redact_id) = &pdu.redacts {
							if self
								.services
								.state_accessor
								.user_can_redact(redact_id, &pdu.sender, &pdu.room_id, false)
								.await?
							{
								self.redact_pdu(redact_id, pdu, shortroomid)
									.await?;
							}
						}
					},
					| _ => {
						let content: RoomRedactionEventContent = pdu.get_content()?;
						if let Some(redact_id) = &content.redacts {
							if self
								.services
								.state_accessor
								.user_can_redact(redact_id, &pdu.sender, &pdu.room_id, false)
								.await?
							{
								self.redact_pdu(redact_id, pdu, shortroomid)
									.await?;
							}
						}
					},
				}
			},
			| TimelineEventType::SpaceChild => {
				if let Some(_state_key) = &pdu.state_key {
					self.services
						.spaces
						.roomid_spacehierarchy_cache
						.lock()
						.await
						.remove(&pdu.room_id);
				}
			},
			| TimelineEventType::RoomMember => {
				if let Some(state_key) = &pdu.state_key {
					// if the state_key fails
					let target_user_id = UserId::parse(state_key)
						.expect("This state_key was previously validated");

					let content: RoomMemberEventContent = pdu.get_content()?;
					let stripped_state = match content.membership {
						| MembershipState::Invite | MembershipState::Knock => self
							.services
							.state
							.summary_stripped(pdu)
							.await
							.into(),
						| _ => None,
					};

					// Update our membership info, we do this here incase a user is invited or
					// knocked and immediately leaves we need the DB to record the invite or
					// knock event for auth
					self.services
						.state_cache
						.update_membership(
							&pdu.room_id,
							target_user_id,
							content,
							&pdu.sender,
							stripped_state,
							None,
							true,
						)
						.await?;
				}
			},
			| TimelineEventType::RoomMessage => {
				let content: ExtractBody = pdu.get_content()?;
				if let Some(body) = content.body {
					self.services
						.search
						.index_pdu(shortroomid, &pdu_id, &body);

					if self
						.services
						.admin
						.is_admin_command(pdu, &body)
						.await
					{
						self.services
							.admin
							.command(body, Some((*pdu.event_id).into()))?;
					}
				}
			},
			| _ => {},
		}

		if let Ok(content) = pdu.get_content::<ExtractRelatesToEventId>() {
			if let Ok(related_pducount) = self
				.get_pdu_count(&content.relates_to.event_id)
				.await
			{
				self.services
					.pdu_metadata
					.add_relation(count2, related_pducount);
			}
		}

		if let Ok(content) = pdu.get_content::<ExtractRelatesTo>() {
			match content.relates_to {
				| Relation::Reply { in_reply_to } => {
					// We need to do it again here, because replies don't have
					// event_id as a top level field
					if let Ok(related_pducount) = self.get_pdu_count(&in_reply_to.event_id).await
					{
						self.services
							.pdu_metadata
							.add_relation(count2, related_pducount);
					}
				},
				| Relation::Thread(thread) => {
					self.services
						.threads
						.add_to_thread(&thread.event_id, pdu)
						.await?;
				},
				| _ => {}, // TODO: Aggregate other types
			}
		}

		for appservice in self.services.appservice.read().await.values() {
			if self
				.services
				.state_cache
				.appservice_in_room(&pdu.room_id, appservice)
				.await
			{
				self.services
					.sending
					.send_pdu_appservice(appservice.registration.id.clone(), pdu_id)?;
				continue;
			}

			// If the RoomMember event has a non-empty state_key, it is targeted at someone.
			// If it is our appservice user, we send this PDU to it.
			if pdu.kind == TimelineEventType::RoomMember {
				if let Some(state_key_uid) = &pdu
					.state_key
					.as_ref()
					.and_then(|state_key| UserId::parse(state_key.as_str()).ok())
				{
					let appservice_uid = appservice.registration.sender_localpart.as_str();
					if state_key_uid == &appservice_uid {
						self.services
							.sending
							.send_pdu_appservice(appservice.registration.id.clone(), pdu_id)?;
						continue;
					}
				}
			}

			let matching_users = |users: &NamespaceRegex| {
				appservice.users.is_match(pdu.sender.as_str())
					|| pdu.kind == TimelineEventType::RoomMember
						&& pdu
							.state_key
							.as_ref()
							.is_some_and(|state_key| users.is_match(state_key))
			};
			let matching_aliases = |aliases: NamespaceRegex| {
				self.services
					.alias
					.local_aliases_for_room(&pdu.room_id)
					.ready_any(move |room_alias| aliases.is_match(room_alias.as_str()))
			};

			if matching_aliases(appservice.aliases.clone()).await
				|| appservice.rooms.is_match(pdu.room_id.as_str())
				|| matching_users(&appservice.users)
			{
				self.services
					.sending
					.send_pdu_appservice(appservice.registration.id.clone(), pdu_id)?;
			}
		}

		Ok(pdu_id)
	}

	pub async fn create_hash_and_sign_event(
		&self,
		pdu_builder: PduBuilder,
		sender: &UserId,
		room_id: &RoomId,
		_mutex_lock: &RoomMutexGuard, /* Take mutex guard to make sure users get the room
		                               * state mutex */
	) -> Result<(PduEvent, CanonicalJsonObject)> {
		let PduBuilder {
			event_type,
			content,
			unsigned,
			state_key,
			redacts,
			timestamp,
		} = pdu_builder;

		let prev_events: Vec<OwnedEventId> = self
			.services
			.state
			.get_forward_extremities(room_id)
			.take(20)
			.map(Into::into)
			.collect()
			.await;

		// If there was no create event yet, assume we are creating a room
		let room_version_id = self
			.services
			.state
			.get_room_version(room_id)
			.await
			.or_else(|_| {
				if event_type == TimelineEventType::RoomCreate {
					let content: RoomCreateEventContent = serde_json::from_str(content.get())?;
					Ok(content.room_version)
				} else {
					Err(Error::InconsistentRoomState(
						"non-create event for room of unknown version",
						room_id.to_owned(),
					))
				}
			})?;

		let room_version = RoomVersion::new(&room_version_id).expect("room version is supported");

		let auth_events = self
			.services
			.state
			.get_auth_events(room_id, &event_type, sender, state_key.as_deref(), &content)
			.await?;

		// Our depth is the maximum depth of prev_events + 1
		let depth = prev_events
			.iter()
			.stream()
			.map(Ok)
			.and_then(|event_id| self.get_pdu(event_id))
			.and_then(|pdu| future::ok(pdu.depth))
			.ignore_err()
			.ready_fold(uint!(0), cmp::max)
			.await
			.saturating_add(uint!(1));

		let mut unsigned = unsigned.unwrap_or_default();

		if let Some(state_key) = &state_key {
			if let Ok(prev_pdu) = self
				.services
				.state_accessor
				.room_state_get(room_id, &event_type.to_string().into(), state_key)
				.await
			{
				unsigned.insert("prev_content".to_owned(), prev_pdu.get_content_as_value());
				unsigned.insert(
					"prev_sender".to_owned(),
					serde_json::to_value(&prev_pdu.sender)
						.expect("UserId::to_value always works"),
				);
				unsigned.insert(
					"replaces_state".to_owned(),
					serde_json::to_value(&prev_pdu.event_id).expect("EventId is valid json"),
				);
			}
		}

		let mut pdu = PduEvent {
			event_id: ruma::event_id!("$thiswillbefilledinlater").into(),
			room_id: room_id.to_owned(),
			sender: sender.to_owned(),
			origin: None,
			origin_server_ts: timestamp.map_or_else(
				|| {
					utils::millis_since_unix_epoch()
						.try_into()
						.expect("u64 fits into UInt")
				},
				|ts| ts.get(),
			),
			kind: event_type,
			content,
			state_key,
			prev_events,
			depth,
			auth_events: auth_events
				.values()
				.map(|pdu| pdu.event_id.clone())
				.collect(),
			redacts,
			unsigned: if unsigned.is_empty() {
				None
			} else {
				Some(to_raw_value(&unsigned).expect("to_raw_value always works"))
			},
			hashes: EventHash { sha256: "aaa".to_owned() },
			signatures: None,
		};

		let auth_fetch = |k: &StateEventType, s: &str| {
			let key = (k.clone(), s.into());
			ready(auth_events.get(&key))
		};

		let auth_check = state_res::auth_check(
			&room_version,
			&pdu,
			None, // TODO: third_party_invite
			auth_fetch,
		)
		.await
		.map_err(|e| err!(Request(Forbidden(warn!("Auth check failed: {e:?}")))))?;

		if !auth_check {
			return Err!(Request(Forbidden("Event is not authorized.")));
		}

		// Hash and sign
		let mut pdu_json = utils::to_canonical_object(&pdu).map_err(|e| {
			err!(Request(BadJson(warn!("Failed to convert PDU to canonical JSON: {e}"))))
		})?;

		// room v3 and above removed the "event_id" field from remote PDU format
		match room_version_id {
			| RoomVersionId::V1 | RoomVersionId::V2 => {},
			| _ => {
				pdu_json.remove("event_id");
			},
		}

		// Add origin because synapse likes that (and it's required in the spec)
		pdu_json.insert(
			"origin".to_owned(),
			to_canonical_value(self.services.globals.server_name())
				.expect("server name is a valid CanonicalJsonValue"),
		);

		if let Err(e) = self
			.services
			.server_keys
			.hash_and_sign_event(&mut pdu_json, &room_version_id)
		{
			return match e {
				| Error::Signatures(ruma::signatures::Error::PduSize) => {
					Err!(Request(TooLarge("Message/PDU is too long (exceeds 65535 bytes)")))
				},
				| _ => Err!(Request(Unknown(warn!("Signing event failed: {e}")))),
			};
		}

		// Generate event id
		pdu.event_id = gen_event_id(&pdu_json, &room_version_id)?;

		pdu_json
			.insert("event_id".into(), CanonicalJsonValue::String(pdu.event_id.clone().into()));

		// Generate short event id
		let _shorteventid = self
			.services
			.short
			.get_or_create_shorteventid(&pdu.event_id)
			.await;

		Ok((pdu, pdu_json))
	}

	/// Creates a new persisted data unit and adds it to a room. This function
	/// takes a roomid_mutex_state, meaning that only this function is able to
	/// mutate the room state.
	#[tracing::instrument(skip(self, state_lock), level = "debug")]
	pub async fn build_and_append_pdu(
		&self,
		pdu_builder: PduBuilder,
		sender: &UserId,
		room_id: &RoomId,
		state_lock: &RoomMutexGuard,
	) -> Result<OwnedEventId> {
		let (pdu, pdu_json) = self
			.create_hash_and_sign_event(pdu_builder, sender, room_id, state_lock)
			.await?;

		if self
			.services
			.admin
			.is_admin_room(&pdu.room_id)
			.await
		{
			self.check_pdu_for_admin_room(&pdu, sender)
				.boxed()
				.await?;
		}

		// If redaction event is not authorized, do not append it to the timeline
		if pdu.kind == TimelineEventType::RoomRedaction {
			use RoomVersionId::*;
			match self
				.services
				.state
				.get_room_version(&pdu.room_id)
				.await?
			{
				| V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
					if let Some(redact_id) = &pdu.redacts {
						if !self
							.services
							.state_accessor
							.user_can_redact(redact_id, &pdu.sender, &pdu.room_id, false)
							.await?
						{
							return Err!(Request(Forbidden("User cannot redact this event.")));
						}
					}
				},
				| _ => {
					let content: RoomRedactionEventContent = pdu.get_content()?;
					if let Some(redact_id) = &content.redacts {
						if !self
							.services
							.state_accessor
							.user_can_redact(redact_id, &pdu.sender, &pdu.room_id, false)
							.await?
						{
							return Err!(Request(Forbidden("User cannot redact this event.")));
						}
					}
				},
			}
		}

		if pdu.kind == TimelineEventType::RoomMember {
			let content: RoomMemberEventContent = pdu.get_content()?;

			if content.join_authorized_via_users_server.is_some()
				&& content.membership != MembershipState::Join
			{
				return Err!(Request(BadJson(
					"join_authorised_via_users_server is only for member joins"
				)));
			}

			if content
				.join_authorized_via_users_server
				.as_ref()
				.is_some_and(|authorising_user| {
					!self
						.services
						.globals
						.user_is_local(authorising_user)
				}) {
				return Err!(Request(InvalidParam(
					"Authorising user does not belong to this homeserver"
				)));
			}
		}

		// We append to state before appending the pdu, so we don't have a moment in
		// time with the pdu without it's state. This is okay because append_pdu can't
		// fail.
		let statehashid = self.services.state.append_to_state(&pdu).await?;

		let pdu_id = self
			.append_pdu(
				&pdu,
				pdu_json,
				// Since this PDU references all pdu_leaves we can update the leaves
				// of the room
				once(pdu.event_id.borrow()),
				state_lock,
			)
			.boxed()
			.await?;

		// We set the room state after inserting the pdu, so that we never have a moment
		// in time where events in the current room state do not exist
		self.services
			.state
			.set_room_state(&pdu.room_id, statehashid, state_lock);

		let mut servers: HashSet<OwnedServerName> = self
			.services
			.state_cache
			.room_servers(&pdu.room_id)
			.map(ToOwned::to_owned)
			.collect()
			.await;

		// In case we are kicking or banning a user, we need to inform their server of
		// the change
		if pdu.kind == TimelineEventType::RoomMember {
			if let Some(state_key_uid) = &pdu
				.state_key
				.as_ref()
				.and_then(|state_key| UserId::parse(state_key.as_str()).ok())
			{
				servers.insert(state_key_uid.server_name().to_owned());
			}
		}

		// Remove our server from the server list since it will be added to it by
		// room_servers() and/or the if statement above
		servers.remove(self.services.globals.server_name());

		self.services
			.sending
			.send_pdu_servers(servers.iter().map(AsRef::as_ref).stream(), &pdu_id)
			.await?;

		Ok(pdu.event_id)
	}

	/// Append the incoming event setting the state snapshot to the state from
	/// the server that sent the event.
	#[tracing::instrument(level = "debug", skip_all)]
	pub async fn append_incoming_pdu<'a, Leafs>(
		&'a self,
		pdu: &'a PduEvent,
		pdu_json: CanonicalJsonObject,
		new_room_leafs: Leafs,
		state_ids_compressed: Arc<CompressedState>,
		soft_fail: bool,
		state_lock: &'a RoomMutexGuard,
	) -> Result<Option<RawPduId>>
	where
		Leafs: Iterator<Item = &'a EventId> + Send + 'a,
	{
		// We append to state before appending the pdu, so we don't have a moment in
		// time with the pdu without it's state. This is okay because append_pdu can't
		// fail.
		self.services
			.state
			.set_event_state(&pdu.event_id, &pdu.room_id, state_ids_compressed)
			.await?;

		if soft_fail {
			self.services
				.pdu_metadata
				.mark_as_referenced(&pdu.room_id, pdu.prev_events.iter().map(AsRef::as_ref));

			self.services
				.state
				.set_forward_extremities(&pdu.room_id, new_room_leafs, state_lock)
				.await;

			return Ok(None);
		}

		let pdu_id = self
			.append_pdu(pdu, pdu_json, new_room_leafs, state_lock)
			.await?;

		Ok(Some(pdu_id))
	}

	/// Returns an iterator over all PDUs in a room. Unknown rooms produce no
	/// items.
	#[inline]
	pub fn all_pdus<'a>(
		&'a self,
		user_id: &'a UserId,
		room_id: &'a RoomId,
	) -> impl Stream<Item = PdusIterItem> + Send + 'a {
		self.pdus(Some(user_id), room_id, None)
			.ignore_err()
	}

	/// Reverse iteration starting at from.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn pdus_rev<'a>(
		&'a self,
		user_id: Option<&'a UserId>,
		room_id: &'a RoomId,
		until: Option<PduCount>,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.db
			.pdus_rev(user_id, room_id, until.unwrap_or_else(PduCount::max))
	}

	/// Forward iteration starting at from.
	#[tracing::instrument(skip(self), level = "debug")]
	pub fn pdus<'a>(
		&'a self,
		user_id: Option<&'a UserId>,
		room_id: &'a RoomId,
		from: Option<PduCount>,
	) -> impl Stream<Item = Result<PdusIterItem>> + Send + 'a {
		self.db
			.pdus(user_id, room_id, from.unwrap_or_else(PduCount::min))
	}

	/// Replace a PDU with the redacted form.
	#[tracing::instrument(name = "redact", level = "debug", skip(self))]
	pub async fn redact_pdu(
		&self,
		event_id: &EventId,
		reason: &PduEvent,
		shortroomid: ShortRoomId,
	) -> Result {
		// TODO: Don't reserialize, keep original json
		let Ok(pdu_id) = self.get_pdu_id(event_id).await else {
			// If event does not exist, just noop
			return Ok(());
		};

		let mut pdu = self.get_pdu_from_id(&pdu_id).await.map_err(|e| {
			err!(Database(error!(?pdu_id, ?event_id, ?e, "PDU ID points to invalid PDU.")))
		})?;

		if let Ok(content) = pdu.get_content::<ExtractBody>() {
			if let Some(body) = content.body {
				self.services
					.search
					.deindex_pdu(shortroomid, &pdu_id, &body);
			}
		}

		let room_version_id = self
			.services
			.state
			.get_room_version(&pdu.room_id)
			.await?;

		pdu.redact(&room_version_id, reason)?;

		let obj = utils::to_canonical_object(&pdu).map_err(|e| {
			err!(Database(error!(?event_id, ?e, "Failed to convert PDU to canonical JSON")))
		})?;

		self.replace_pdu(&pdu_id, &obj, &pdu).await
	}

	#[tracing::instrument(name = "backfill", level = "debug", skip(self))]
	pub async fn backfill_if_required(&self, room_id: &RoomId, from: PduCount) -> Result<()> {
		if self
			.services
			.state_cache
			.room_joined_count(room_id)
			.await
			.is_ok_and(|count| count <= 1)
			&& !self
				.services
				.state_accessor
				.is_world_readable(room_id)
				.await
		{
			// Room is empty (1 user or none), there is no one that can backfill
			return Ok(());
		}

		let first_pdu = self
			.first_item_in_room(room_id)
			.await
			.expect("Room is not empty");

		if first_pdu.0 < from {
			// No backfill required, there are still events between them
			return Ok(());
		}

		let power_levels: RoomPowerLevelsEventContent = self
			.services
			.state_accessor
			.room_state_get_content(room_id, &StateEventType::RoomPowerLevels, "")
			.await
			.unwrap_or_default();

		let room_mods = power_levels
			.users
			.iter()
			.filter_map(|(user_id, level)| {
				if level > &power_levels.users_default
					&& !self.services.globals.user_is_local(user_id)
				{
					Some(user_id.server_name())
				} else {
					None
				}
			});

		let canonical_room_alias_server = once(
			self.services
				.state_accessor
				.get_canonical_alias(room_id)
				.await,
		)
		.filter_map(Result::ok)
		.map(|alias| alias.server_name().to_owned())
		.stream();

		let mut servers = room_mods
			.stream()
			.map(ToOwned::to_owned)
			.chain(canonical_room_alias_server)
			.chain(
				self.services
					.server
					.config
					.trusted_servers
					.iter()
					.map(ToOwned::to_owned)
					.stream(),
			)
			.ready_filter(|server_name| !self.services.globals.server_is_ours(server_name))
			.filter_map(|server_name| async move {
				self.services
					.state_cache
					.server_in_room(&server_name, room_id)
					.await
					.then_some(server_name)
			})
			.boxed();

		while let Some(ref backfill_server) = servers.next().await {
			info!("Asking {backfill_server} for backfill");
			let response = self
				.services
				.sending
				.send_federation_request(
					backfill_server,
					federation::backfill::get_backfill::v1::Request {
						room_id: room_id.to_owned(),
						v: vec![first_pdu.1.event_id.clone()],
						limit: uint!(100),
					},
				)
				.await;
			match response {
				| Ok(response) => {
					for pdu in response.pdus {
						if let Err(e) = self
							.backfill_pdu(backfill_server, pdu)
							.boxed()
							.await
						{
							debug_warn!("Failed to add backfilled pdu in room {room_id}: {e}");
						}
					}
					return Ok(());
				},
				| Err(e) => {
					warn!("{backfill_server} failed to provide backfill for room {room_id}: {e}");
				},
			}
		}

		info!("No servers could backfill, but backfill was needed in room {room_id}");
		Ok(())
	}

	#[tracing::instrument(skip(self, pdu), level = "debug")]
	pub async fn backfill_pdu(&self, origin: &ServerName, pdu: Box<RawJsonValue>) -> Result<()> {
		let (room_id, event_id, value) = self
			.services
			.event_handler
			.parse_incoming_pdu(&pdu)
			.await?;

		// Lock so we cannot backfill the same pdu twice at the same time
		let mutex_lock = self
			.services
			.event_handler
			.mutex_federation
			.lock(&room_id)
			.await;

		// Skip the PDU if we already have it as a timeline event
		if let Ok(pdu_id) = self.get_pdu_id(&event_id).await {
			debug!("We already know {event_id} at {pdu_id:?}");
			return Ok(());
		}

		self.services
			.event_handler
			.handle_incoming_pdu(origin, &room_id, &event_id, value, false)
			.boxed()
			.await?;

		let value = self.get_pdu_json(&event_id).await?;

		let pdu = self.get_pdu(&event_id).await?;

		let shortroomid = self
			.services
			.short
			.get_shortroomid(&room_id)
			.await?;

		let insert_lock = self.mutex_insert.lock(&room_id).await;

		let count: i64 = self
			.services
			.globals
			.next_count()
			.unwrap()
			.try_into()?;

		let pdu_id: RawPduId = PduId {
			shortroomid,
			shorteventid: PduCount::Backfilled(validated!(0 - count)),
		}
		.into();

		// Insert pdu
		self.db
			.prepend_backfill_pdu(&pdu_id, &event_id, &value);

		drop(insert_lock);

		if pdu.kind == TimelineEventType::RoomMessage {
			let content: ExtractBody = pdu.get_content()?;
			if let Some(body) = content.body {
				self.services
					.search
					.index_pdu(shortroomid, &pdu_id, &body);
			}
		}
		drop(mutex_lock);

		debug!("Prepended backfill pdu");
		Ok(())
	}
}

#[implement(Service)]
#[tracing::instrument(skip_all, level = "debug")]
async fn check_pdu_for_admin_room(&self, pdu: &PduEvent, sender: &UserId) -> Result<()> {
	match &pdu.kind {
		| TimelineEventType::RoomEncryption => {
			return Err!(Request(Forbidden(error!("Encryption not supported in admins room."))));
		},
		| TimelineEventType::RoomMember => {
			let target = pdu
				.state_key()
				.filter(|v| v.starts_with('@'))
				.unwrap_or(sender.as_str());

			let server_user = &self.services.globals.server_user.to_string();

			let content: RoomMemberEventContent = pdu.get_content()?;
			match content.membership {
				| MembershipState::Leave => {
					if target == server_user {
						return Err!(Request(Forbidden(error!(
							"Server user cannot leave the admins room."
						))));
					}

					let count = self
						.services
						.state_cache
						.room_members(&pdu.room_id)
						.ready_filter(|user| self.services.globals.user_is_local(user))
						.ready_filter(|user| *user != target)
						.boxed()
						.count()
						.await;

					if count < 2 {
						return Err!(Request(Forbidden(error!(
							"Last admin cannot leave the admins room."
						))));
					}
				},

				| MembershipState::Ban if pdu.state_key().is_some() => {
					if target == server_user {
						return Err!(Request(Forbidden(error!(
							"Server cannot be banned from admins room."
						))));
					}

					let count = self
						.services
						.state_cache
						.room_members(&pdu.room_id)
						.ready_filter(|user| self.services.globals.user_is_local(user))
						.ready_filter(|user| *user != target)
						.boxed()
						.count()
						.await;

					if count < 2 {
						return Err!(Request(Forbidden(error!(
							"Last admin cannot be banned from admins room."
						))));
					}
				},
				| _ => {},
			}
		},
		| _ => {},
	}

	Ok(())
}
