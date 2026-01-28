use std::{
	collections::HashMap,
	fmt::Write,
	iter::once,
	str::FromStr,
	time::{Instant, SystemTime},
};

use futures::{FutureExt, StreamExt, TryStreamExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedRoomId,
	OwnedRoomOrAliasId, OwnedServerName, RoomId, RoomVersionId,
	api::federation::event::get_room_state, events::AnyStateEvent, serde::Raw,
};
use serde::Serialize;
use tracing_subscriber::EnvFilter;
use tuwunel_core::{
	Err, Result, debug_error, err, info, jwt,
	matrix::{
		Event,
		pdu::{PduEvent, PduId, RawPduId},
	},
	trace, utils,
	utils::{
		stream::{IterStream, ReadyExt},
		string::EMPTY,
		time::now_secs,
	},
	warn,
};
use tuwunel_service::rooms::{short::ShortRoomId, state_compressor::HashSetCompressStateEvent};

use crate::admin_command;

#[admin_command]
pub(super) async fn echo(&self, message: Vec<String>) -> Result {
	let message = message.join(" ");
	self.write_str(&message).await
}

#[admin_command]
pub(super) async fn get_auth_chain(&self, event_id: OwnedEventId) -> Result {
	let Ok(event) = self
		.services
		.timeline
		.get_pdu_json(&event_id)
		.await
	else {
		return Err!("Event not found.");
	};

	let room_id_str = event
		.get("room_id")
		.and_then(CanonicalJsonValue::as_str)
		.ok_or_else(|| err!(Database("Invalid event in database")))?;

	let room_id = <&RoomId>::try_from(room_id_str)
		.map_err(|_| err!(Database("Invalid room id field in event in database")))?;

	let room_version = self
		.services
		.state
		.get_room_version(room_id)
		.await?;

	let start = Instant::now();
	let count = self
		.services
		.auth_chain
		.event_ids_iter(room_id, &room_version, once(event_id.as_ref()))
		.ready_filter_map(Result::ok)
		.count()
		.await;

	let elapsed = start.elapsed();
	let out = format!("Loaded auth chain with length {count} in {elapsed:?}");

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn parse_pdu(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&EMPTY).trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.");
	}

	let string = self.body[1..self.body.len().saturating_sub(1)].join("\n");
	let rules = RoomVersionId::V6
		.rules()
		.expect("rules for V6 rooms");
	match serde_json::from_str(&string) {
		| Err(e) => return Err!("Invalid json in command body: {e}"),
		| Ok(value) => match ruma::signatures::reference_hash(&value, &rules) {
			| Err(e) => return Err!("Could not parse PDU JSON: {e:?}"),
			| Ok(hash) => {
				let event_id = OwnedEventId::parse(format!("${hash}"));
				match serde_json::from_value::<PduEvent>(serde_json::to_value(value)?) {
					| Err(e) => return Err!("EventId: {event_id:?}\nCould not parse event: {e}"),
					| Ok(pdu) => write!(self, "EventId: {event_id:?}\n{pdu:#?}"),
				}
			},
		},
	}
	.await
}

#[admin_command]
pub(super) async fn get_pdu(&self, event_id: OwnedEventId) -> Result {
	let mut outlier = false;
	let mut pdu_json = self
		.services
		.timeline
		.get_non_outlier_pdu_json(&event_id)
		.await;

	if pdu_json.is_err() {
		outlier = true;
		pdu_json = self
			.services
			.timeline
			.get_pdu_json(&event_id)
			.await;
	}

	match pdu_json {
		| Err(_) => return Err!("PDU not found locally."),
		| Ok(json) => {
			let text = serde_json::to_string_pretty(&json)?;
			let msg = if outlier {
				"Outlier (Rejected / Soft Failed) PDU found in our database"
			} else {
				"PDU found in our database"
			};
			write!(self, "{msg}\n```json\n{text}\n```",)
		},
	}
	.await
}

#[admin_command]
pub(super) async fn get_short_pdu(&self, shortroomid: ShortRoomId, count: i64) -> Result {
	let pdu_id: RawPduId = PduId { shortroomid, count: count.into() }.into();

	let pdu_json = self
		.services
		.timeline
		.get_pdu_json_from_id(&pdu_id)
		.await;

	match pdu_json {
		| Err(_) => return Err!("PDU not found locally."),
		| Ok(json) => {
			let json_text = serde_json::to_string_pretty(&json)?;
			write!(self, "```json\n{json_text}\n```")
		},
	}
	.await
}

#[admin_command]
pub(super) async fn get_remote_pdu_list(&self, server: OwnedServerName, force: bool) -> Result {
	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.",);
	}

	if server == self.services.globals.server_name() {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs from the database.",
		);
	}

	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&EMPTY).trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.",);
	}

	let list = self
		.body
		.iter()
		.collect::<Vec<_>>()
		.drain(1..self.body.len().saturating_sub(1))
		.filter_map(|pdu| EventId::parse(pdu).ok())
		.collect::<Vec<_>>();

	let mut failed_count: usize = 0;
	let mut success_count: usize = 0;

	for event_id in list {
		if force {
			match self
				.get_remote_pdu(event_id.to_owned(), server.clone())
				.await
			{
				| Err(e) => {
					failed_count = failed_count.saturating_add(1);
					self.services
						.admin
						.send_text(&format!("Failed to get remote PDU, ignoring error: {e}"))
						.await;

					warn!("Failed to get remote PDU, ignoring error: {e}");
				},
				| _ => {
					success_count = success_count.saturating_add(1);
				},
			}
		} else {
			self.get_remote_pdu(event_id.to_owned(), server.clone())
				.await?;
			success_count = success_count.saturating_add(1);
		}
	}

	let out =
		format!("Fetched {success_count} remote PDUs successfully with {failed_count} failures");

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn get_remote_pdu(
	&self,
	event_id: OwnedEventId,
	server: OwnedServerName,
) -> Result {
	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.");
	}

	if server == self.services.globals.server_name() {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs.",
		);
	}

	match self
		.services
		.federation
		.execute(&server, ruma::api::federation::event::get_event::v1::Request {
			event_id: event_id.clone(),
		})
		.await
	{
		| Err(e) => {
			return Err!(
				"Remote server did not have PDU or failed sending request to remote server: {e}"
			);
		},
		| Ok(response) => {
			let json: CanonicalJsonObject =
				serde_json::from_str(response.pdu.get()).map_err(|e| {
					warn!(
						"Requested event ID {event_id} from server but failed to convert from \
						 RawValue to CanonicalJsonObject (malformed event/response?): {e}"
					);
					err!(Request(Unknown(
						"Received response from server but failed to parse PDU"
					)))
				})?;

			trace!("Attempting to parse PDU: {:?}", &response.pdu);
			let (room_id, ..) = {
				let parsed_result = self
					.services
					.event_handler
					.parse_incoming_pdu(&response.pdu)
					.boxed()
					.await;

				match parsed_result {
					| Ok(t) => t,
					| Err(e) => {
						warn!("Failed to parse PDU: {e}");
						info!("Full PDU: {:?}", &response.pdu);
						return Err!("Failed to parse PDU remote server {server} sent us: {e}");
					},
				}
			};

			info!("Attempting to handle event ID {event_id} as backfilled PDU");
			self.services
				.timeline
				.backfill_pdu(&room_id, &server, response.pdu)
				.await?;

			let text = serde_json::to_string_pretty(&json)?;
			let msg = "Got PDU from specified server and handled as backfilled";
			write!(self, "{msg}. Event body:\n```json\n{text}\n```")
		},
	}
	.await
}

#[admin_command]
pub(super) async fn get_room_state(&self, room: OwnedRoomOrAliasId) -> Result {
	let room_id = self.services.alias.maybe_resolve(&room).await?;
	let room_state: Vec<Raw<AnyStateEvent>> = self
		.services
		.state_accessor
		.room_state_full_pdus(&room_id)
		.map_ok(Event::into_format)
		.try_collect()
		.await?;

	if room_state.is_empty() {
		return Err!("Unable to find room state in our database (vector is empty)",);
	}

	let json = serde_json::to_string_pretty(&room_state).map_err(|e| {
		err!(Database(
			"Failed to convert room state events to pretty JSON, possible invalid room state \
			 events in our database {e}",
		))
	})?;

	let out = format!("```json\n{json}\n```");
	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn ping(&self, server: OwnedServerName) -> Result {
	if server == self.services.globals.server_name() {
		return Err!("Not allowed to send federation requests to ourselves.");
	}

	let timer = tokio::time::Instant::now();

	match self
		.services
		.federation
		.execute(&server, ruma::api::federation::discovery::get_server_version::v1::Request {})
		.await
	{
		| Err(e) => {
			return Err!("Failed sending federation request to specified server:\n\n{e}");
		},
		| Ok(response) => {
			let ping_time = timer.elapsed();
			let json_text_res = serde_json::to_string_pretty(&response.server);

			let out = if let Ok(json) = json_text_res {
				format!("Got response which took {ping_time:?} time:\n```json\n{json}\n```")
			} else {
				format!("Got non-JSON response which took {ping_time:?} time:\n{response:?}")
			};

			write!(self, "{out}")
		},
	}
	.await
}

#[admin_command]
pub(super) async fn force_device_list_updates(&self) -> Result {
	// Force E2EE device list updates for all users
	self.services
		.users
		.stream()
		.for_each(|user_id| {
			self.services
				.users
				.mark_device_key_update(user_id)
		})
		.await;

	write!(self, "Marked all devices for all users as having new keys to update").await
}

#[admin_command]
pub(super) async fn change_log_level(&self, filter: Option<String>, reset: bool) -> Result {
	let handles = &["console"];

	if reset {
		let old_filter_layer = match EnvFilter::try_new(&self.services.server.config.log) {
			| Ok(s) => s,
			| Err(e) => return Err!("Log level from config appears to be invalid now: {e}"),
		};

		match self
			.services
			.server
			.log
			.reload
			.reload(&old_filter_layer, Some(handles))
		{
			| Err(e) => {
				return Err!("Failed to modify and reload the global tracing log level: {e}");
			},
			| Ok(()) => {
				let value = &self.services.server.config.log;
				let out = format!("Successfully changed log level back to config value {value}");
				return self.write_str(&out).await;
			},
		}
	}

	if let Some(filter) = filter {
		let new_filter_layer = match EnvFilter::try_new(filter) {
			| Ok(s) => s,
			| Err(e) => return Err!("Invalid log level filter specified: {e}"),
		};

		match self
			.services
			.server
			.log
			.reload
			.reload(&new_filter_layer, Some(handles))
		{
			| Ok(()) => {
				return self
					.write_str("Successfully changed log level")
					.await;
			},
			| Err(e) => {
				return Err!("Failed to modify and reload the global tracing log level: {e}");
			},
		}
	}

	Err!("No log level was specified.")
}

#[admin_command]
pub(super) async fn sign_json(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.");
	}

	let string = self.body[1..self.body.len().checked_sub(1).unwrap()].join("\n");
	match serde_json::from_str(&string) {
		| Err(e) => return Err!("Invalid json: {e}"),
		| Ok(mut value) => {
			self.services.server_keys.sign_json(&mut value)?;
			let json_text = serde_json::to_string_pretty(&value)?;
			write!(self, "{json_text}")
		},
	}
	.await
}

#[admin_command]
pub(super) async fn verify_json(&self) -> Result {
	if self.body.len() < 2
		|| !self.body[0].trim().starts_with("```")
		|| self.body.last().unwrap_or(&"").trim() != "```"
	{
		return Err!("Expected code block in command body. Add --help for details.");
	}

	let string = self.body[1..self.body.len().checked_sub(1).unwrap()].join("\n");
	match serde_json::from_str::<CanonicalJsonObject>(&string) {
		| Err(e) => return Err!("Invalid json: {e}"),
		| Ok(value) => match self
			.services
			.server_keys
			.verify_json(&value, None)
			.await
		{
			| Err(e) => return Err!("Signature verification failed: {e}"),
			| Ok(()) => write!(self, "Signature correct"),
		},
	}
	.await
}

#[admin_command]
pub(super) async fn verify_pdu(&self, event_id: OwnedEventId) -> Result {
	use ruma::signatures::Verified;

	let mut event = self
		.services
		.timeline
		.get_pdu_json(&event_id)
		.await?;

	event.remove("event_id");
	let msg = match self
		.services
		.server_keys
		.verify_event(&event, None)
		.await
	{
		| Err(e) => return Err(e),
		| Ok(Verified::Signatures) => "signatures OK, but content hash failed (redaction).",
		| Ok(Verified::All) => "signatures and hashes OK.",
	};

	self.write_str(msg).await
}

#[admin_command]
#[tracing::instrument(skip(self))]
pub(super) async fn first_pdu_in_room(&self, room_id: OwnedRoomId) -> Result {
	if !self
		.services
		.state_cache
		.server_in_room(&self.services.server.name, &room_id)
		.await
	{
		return Err!("We are not participating in the room / we don't know about the room ID.",);
	}

	let first_pdu = self
		.services
		.timeline
		.first_pdu_in_room(&room_id)
		.await
		.map_err(|_| err!(Database("Failed to find the first PDU in database")))?;

	let out = format!("{first_pdu:?}");
	self.write_str(&out).await
}

#[admin_command]
#[tracing::instrument(skip(self))]
pub(super) async fn latest_pdu_in_room(&self, room_id: OwnedRoomId) -> Result {
	if !self
		.services
		.state_cache
		.server_in_room(&self.services.server.name, &room_id)
		.await
	{
		return Err!("We are not participating in the room / we don't know about the room ID.");
	}

	let latest_pdu = self
		.services
		.timeline
		.latest_pdu_in_room(&room_id)
		.await
		.map_err(|_| err!(Database("Failed to find the latest PDU in database")))?;

	let out = format!("{latest_pdu:?}");
	self.write_str(&out).await
}

#[admin_command]
#[tracing::instrument(skip(self))]
pub(super) async fn force_set_room_state_from_server(
	&self,
	room_id: OwnedRoomId,
	server_name: OwnedServerName,
) -> Result {
	if !self
		.services
		.state_cache
		.server_in_room(&self.services.server.name, &room_id)
		.await
	{
		return Err!("We are not participating in the room / we don't know about the room ID.");
	}

	let first_pdu = self
		.services
		.timeline
		.latest_pdu_in_room(&room_id)
		.await
		.map_err(|_| err!(Database("Failed to find the latest PDU in database")))?;

	let room_version = self
		.services
		.state
		.get_room_version(&room_id)
		.await?;

	let mut state: HashMap<u64, OwnedEventId> = HashMap::new();

	let remote_state_response = self
		.services
		.federation
		.execute(&server_name, get_room_state::v1::Request {
			room_id: room_id.clone(),
			event_id: first_pdu.event_id().to_owned(),
		})
		.await?;

	for pdu in remote_state_response.pdus.clone() {
		match self
			.services
			.event_handler
			.parse_incoming_pdu(&pdu)
			.await
		{
			| Ok(t) => t,
			| Err(e) => {
				warn!("Could not parse PDU, ignoring: {e}");
				continue;
			},
		};
	}

	info!("Going through room_state response PDUs");
	for result in remote_state_response.pdus.iter().map(|pdu| {
		self.services
			.server_keys
			.validate_and_add_event_id(pdu, &room_version)
	}) {
		let Ok((event_id, mut value)) = result.await else {
			continue;
		};

		let invalid_pdu_err = |e| {
			debug_error!("Invalid PDU in fetching remote room state PDUs response: {value:#?}");
			err!(BadServerResponse(debug_error!("Invalid PDU in send_join response: {e:?}")))
		};

		let pdu = if value["type"] == "m.room.create" {
			PduEvent::from_rid_val(&room_id, &event_id, value.clone()).map_err(invalid_pdu_err)?
		} else {
			PduEvent::from_id_val(&event_id, value.clone()).map_err(invalid_pdu_err)?
		};

		if !value.contains_key("room_id") {
			let room_id = CanonicalJsonValue::String(room_id.as_str().into());
			value.insert("room_id".into(), room_id);
		}

		self.services
			.timeline
			.add_pdu_outlier(&event_id, &value);

		if let Some(state_key) = &pdu.state_key {
			let shortstatekey = self
				.services
				.short
				.get_or_create_shortstatekey(&pdu.kind.to_string().into(), state_key)
				.await;

			state.insert(shortstatekey, pdu.event_id.clone());
		}
	}

	info!("Going through auth_chain response");
	for result in remote_state_response
		.auth_chain
		.iter()
		.map(|pdu| {
			self.services
				.server_keys
				.validate_and_add_event_id(pdu, &room_version)
		}) {
		let Ok((event_id, value)) = result.await else {
			continue;
		};

		self.services
			.timeline
			.add_pdu_outlier(&event_id, &value);
	}

	let new_room_state = self
		.services
		.event_handler
		.resolve_state(&room_id, &room_version, state)
		.await?;

	info!("Forcing new room state");
	let HashSetCompressStateEvent {
		shortstatehash: short_state_hash,
		added,
		removed,
	} = self
		.services
		.state_compressor
		.save_state(room_id.clone().as_ref(), new_room_state)
		.await?;

	let state_lock = self.services.state.mutex.lock(&*room_id).await;

	self.services
		.state
		.force_state(room_id.clone().as_ref(), short_state_hash, added, removed, &state_lock)
		.await?;

	info!(
		"Updating joined counts for room just in case (e.g. we may have found a difference in \
		 the room's m.room.member state"
	);
	self.services
		.state_cache
		.update_joined_count(&room_id)
		.await;

	self.write_str("Successfully forced the room state from the requested remote server.")
		.await
}

#[admin_command]
pub(super) async fn get_signing_keys(
	&self,
	server_name: Option<OwnedServerName>,
	notary: Option<OwnedServerName>,
	query: bool,
) -> Result {
	let server_name = server_name.unwrap_or_else(|| self.services.server.name.clone());

	if let Some(notary) = notary {
		let signing_keys = self
			.services
			.server_keys
			.notary_request(&notary, &server_name)
			.await?;

		let out = format!("```rs\n{signing_keys:#?}\n```");
		return self.write_str(&out).await;
	}

	let signing_keys = if query {
		self.services
			.server_keys
			.server_request(&server_name)
			.await?
	} else {
		self.services
			.server_keys
			.signing_keys_for(&server_name)
			.await?
	};

	let out = format!("```rs\n{signing_keys:#?}\n```");
	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn get_verify_keys(&self, server_name: Option<OwnedServerName>) -> Result {
	let server_name = server_name.unwrap_or_else(|| self.services.server.name.clone());

	let keys = self
		.services
		.server_keys
		.verify_keys_for(&server_name)
		.await;

	let mut out = String::new();
	writeln!(out, "| Key ID | Public Key |")?;
	writeln!(out, "| --- | --- |")?;
	for (key_id, key) in keys {
		writeln!(out, "| {key_id} | {key:?} |")?;
	}

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn resolve_true_destination(
	&self,
	server_name: OwnedServerName,
	no_cache: bool,
) -> Result {
	if !self.services.server.config.allow_federation {
		return Err!("Federation is disabled on this homeserver.",);
	}

	if server_name == self.services.server.name {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs.",
		);
	}

	let actual = self
		.services
		.resolver
		.resolve_actual_dest(&server_name, !no_cache)
		.await?;

	let msg = format!("Destination: {}\nHostname URI: {}", actual.dest, actual.host);
	self.write_str(&msg).await
}

#[admin_command]
pub(super) async fn memory_stats(&self, opts: Option<String>) -> Result {
	const OPTS: &str = "abcdefghijklmnopqrstuvwxyz";

	let opts: String = OPTS
		.chars()
		.filter(|&c| {
			let allow_any = opts.as_ref().is_some_and(|opts| opts == "*");

			let allow = allow_any || opts.as_ref().is_some_and(|opts| opts.contains(c));

			!allow
		})
		.collect();

	let stats = tuwunel_core::alloc::memory_stats(&opts).unwrap_or_default();

	self.write_str("```\n").await?;
	self.write_str(&stats).await?;
	self.write_str("\n```").await?;
	Ok(())
}

#[cfg(tokio_unstable)]
#[admin_command]
pub(super) async fn runtime_metrics(&self) -> Result {
	let out = self
		.services
		.server
		.metrics
		.runtime_metrics()
		.map_or_else(
			|| "Runtime metrics are not available.".to_owned(),
			|metrics| {
				format!(
					"```rs\nnum_workers: {}\nnum_alive_tasks: {}\nglobal_queue_depth: {}\n```",
					metrics.num_workers(),
					metrics.num_alive_tasks(),
					metrics.global_queue_depth()
				)
			},
		);

	self.write_str(&out).await
}

#[cfg(not(tokio_unstable))]
#[admin_command]
pub(super) async fn runtime_metrics(&self) -> Result {
	self.write_str("Runtime metrics require building with `tokio_unstable`.")
		.await
}

#[cfg(tokio_unstable)]
#[admin_command]
pub(super) async fn runtime_interval(&self) -> Result {
	let out = self
		.services
		.server
		.metrics
		.runtime_interval()
		.map_or_else(
			|| "Runtime metrics are not available.".to_owned(),
			|metrics| format!("```rs\n{metrics:#?}\n```"),
		);

	self.write_str(&out).await
}

#[cfg(not(tokio_unstable))]
#[admin_command]
pub(super) async fn runtime_interval(&self) -> Result {
	self.write_str("Runtime metrics require building with `tokio_unstable`.")
		.await
}

#[admin_command]
pub(super) async fn time(&self) -> Result {
	let now = SystemTime::now();
	let now = utils::time::format(now, "%+");

	self.write_str(&now).await
}

#[admin_command]
pub(super) async fn list_dependencies(&self, names: bool) -> Result {
	if names {
		let out = info::cargo::dependencies_names().join(" ");
		return self.write_str(&out).await;
	}

	let mut out = String::new();
	let deps = info::cargo::dependencies();
	writeln!(out, "| name | version | features |")?;
	writeln!(out, "| ---- | ------- | -------- |")?;
	for (name, dep) in deps {
		let version = dep.try_req().unwrap_or("*");
		let feats = dep.req_features();
		let feats = if !feats.is_empty() {
			feats.join(" ")
		} else {
			String::new()
		};

		writeln!(out, "| {name} | {version} | {feats} |")?;
	}

	self.write_str(&out).await
}

#[admin_command]
pub(super) async fn database_stats(
	&self,
	property: Option<String>,
	map: Option<String>,
) -> Result {
	let map_name = map.as_ref().map_or(EMPTY, String::as_str);
	let property = property.unwrap_or_else(|| "rocksdb.stats".to_owned());
	self.services
		.db
		.iter()
		.filter(|&(&name, _)| map_name.is_empty() || map_name == name)
		.try_stream()
		.try_for_each(|(&name, map)| {
			let res = map.property(&property).expect("invalid property");
			writeln!(self, "##### {name}:\n```\n{}\n```", res.trim())
		})
		.await
}

#[admin_command]
pub(super) async fn database_files(&self, map: Option<String>, level: Option<i32>) -> Result {
	let mut files: Vec<_> = self
		.services
		.db
		.engine
		.file_list()
		.collect::<Result<_>>()?;

	files.sort_by_key(|f| f.name.clone());

	writeln!(self, "| lev  | sst  | keys | dels | size | column |").await?;
	writeln!(self, "| ---: | :--- | ---: | ---: | ---: | :---   |").await?;
	files
		.into_iter()
		.filter(|file| {
			map.as_deref()
				.is_none_or(|map| map == file.column_family_name)
		})
		.filter(|file| {
			level
				.as_ref()
				.is_none_or(|&level| level == file.level)
		})
		.try_stream()
		.try_for_each(|file| {
			writeln!(
				self,
				"| {} | {:<13} | {:7}+ | {:4}- | {:9} | {} |",
				file.level,
				file.name,
				file.num_entries,
				file.num_deletions,
				file.size,
				file.column_family_name,
			)
		})
		.await
}

#[admin_command]
pub(super) async fn trim_memory(&self) -> Result {
	tuwunel_core::alloc::trim(None)?;

	writeln!(self, "done").await
}

#[admin_command]
pub(super) async fn create_jwt(
	&self,
	user: String,
	exp_from_now: Option<u64>,
	nbf_from_now: Option<u64>,
	issuer: Option<String>,
	audience: Option<String>,
) -> Result {
	use jwt::{Algorithm, EncodingKey, Header, encode};

	#[derive(Serialize)]
	struct Claim {
		sub: String,
		iss: Option<String>,
		aud: Option<String>,
		exp: Option<usize>,
		nbf: Option<usize>,
	}

	let config = &self.services.config.jwt;
	if config.format.as_str() != "HMAC" {
		return Err!("This command only supports HMAC key format, not {}.", config.format);
	}

	let key = EncodingKey::from_secret(config.key.as_ref());
	let alg = Algorithm::from_str(config.algorithm.as_str()).map_err(|e| {
		err!(Config("jwt.algorithm", "JWT algorithm is not recognized or configured {e}"))
	})?;

	let header = Header { alg, ..Default::default() };
	let claim = Claim {
		sub: user,

		iss: issuer,

		aud: audience,

		exp: exp_from_now
			.and_then(|val| now_secs().checked_add(val))
			.map(TryInto::try_into)
			.and_then(Result::ok),

		nbf: nbf_from_now
			.and_then(|val| now_secs().checked_add(val))
			.map(TryInto::try_into)
			.and_then(Result::ok),
	};

	encode(&header, &claim, &key)
		.map_err(|e| err!("Failed to encode JWT: {e}"))
		.map(async |token| self.write_str(&token).await)?
		.await
}

#[admin_command]
pub(super) async fn resync_database(&self) -> Result {
	if !self.services.db.is_secondary() {
		return Err!("Not a secondary instance.");
	}

	self.services
		.db
		.engine
		.update()
		.map_err(|e| err!("Failed to update from primary: {e:?}"))
}

#[admin_command]
pub(super) async fn get_retained_pdu(&self, event_id: OwnedEventId) -> Result {
	let pdu = self
		.services
		.retention
		.get_original_pdu_json(&event_id)
		.await?;

	let text = serde_json::to_string_pretty(&pdu)?;

	self.write_str(&format!("Original PDU:\n```json\n{text}\n```"))
		.await?;

	Ok(())
}
