use std::{collections::BTreeMap, net::IpAddr, time::Instant};

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{
	CanonicalJsonObject, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, ServerName, UserId,
	api::{
		client::error::ErrorKind,
		federation::transactions::{
			edu::{
				DeviceListUpdateContent, DirectDeviceContent, Edu, PresenceContent,
				PresenceUpdate, ReceiptContent, ReceiptData, ReceiptMap, SigningKeyUpdateContent,
				TypingContent,
			},
			send_transaction_message,
		},
	},
	events::receipt::{ReceiptEvent, ReceiptEventContent, ReceiptType},
	serde::Raw,
	to_device::DeviceIdOrAllDevices,
};
use tuwunel_core::{
	Err, Error, Result, debug,
	debug::INFO_SPAN_LEVEL,
	debug_warn, err, error,
	itertools::Itertools,
	result::LogErr,
	trace,
	utils::{
		IterStream, ReadyExt,
		debug::str_truncated,
		millis_since_unix_epoch,
		stream::{BroadbandExt, TryBroadbandExt, automatic_width},
	},
	warn,
};
use tuwunel_service::{
	Services,
	sending::{EDU_LIMIT, PDU_LIMIT},
};

use crate::Ruma;

type ResolvedMap = BTreeMap<OwnedEventId, Result>;
type Pdu = (OwnedRoomId, OwnedEventId, CanonicalJsonObject);

/// # `PUT /_matrix/federation/v1/send/{txnId}`
///
/// Push EDUs and PDUs to this server.
#[tracing::instrument(
	name = "txn",
	level = INFO_SPAN_LEVEL,
	skip_all,
	fields(
		%client,
		origin = body.origin().as_str(),
		txn = str_truncated(body.transaction_id.as_str(), 20),
	),
)]
pub(crate) async fn send_transaction_message_route(
	State(services): State<crate::State>,
	InsecureClientIp(client): InsecureClientIp,
	body: Ruma<send_transaction_message::v1::Request>,
) -> Result<send_transaction_message::v1::Response> {
	if body.origin() != body.body.origin {
		return Err!(Request(Forbidden(
			"Not allowed to send transactions on behalf of other servers"
		)));
	}

	if body.pdus.len() > PDU_LIMIT {
		return Err!(Request(Forbidden(
			"Not allowed to send more than {PDU_LIMIT} PDUs in one transaction"
		)));
	}

	if body.edus.len() > EDU_LIMIT {
		return Err!(Request(Forbidden(
			"Not allowed to send more than {EDU_LIMIT} EDUs in one transaction"
		)));
	}

	let txn_start_time = Instant::now();
	trace!(
		pdus = body.pdus.len(),
		edus = body.edus.len(),
		elapsed = ?txn_start_time.elapsed(),
		"Starting txn",
	);

	let pdus = body
		.pdus
		.iter()
		.stream()
		.broad_then(|pdu| services.event_handler.parse_incoming_pdu(pdu))
		.inspect_err(|e| debug_warn!("Could not parse PDU: {e}"))
		.ready_filter_map(Result::ok);

	let edus = body
		.edus
		.iter()
		.map(|edu| edu.json().get())
		.map(serde_json::from_str)
		.filter_map(Result::ok)
		.stream();

	trace!(
		elapsed = ?txn_start_time.elapsed(),
		"Parsed txn",
	);

	let results = handle(&services, &client, body.origin(), txn_start_time, pdus, edus).await?;

	debug!(
		pdus = body.pdus.len(),
		edus = body.edus.len(),
		elapsed = ?txn_start_time.elapsed(),
		"Finished txn",
	);

	for (id, result) in &results {
		if let Err(e) = result
			&& matches!(e, Error::BadRequest(ErrorKind::NotFound, _))
		{
			warn!("Incoming PDU failed {id}: {e:?}");
		}
	}

	Ok(send_transaction_message::v1::Response {
		pdus: results
			.into_iter()
			.map(|(e, r)| (e, r.map_err(error::sanitized_message)))
			.collect(),
	})
}

async fn handle(
	services: &Services,
	client: &IpAddr,
	origin: &ServerName,
	started: Instant,
	pdus: impl Stream<Item = Pdu> + Send,
	edus: impl Stream<Item = Edu> + Send,
) -> Result<ResolvedMap> {
	// group pdus by room
	let pdus = pdus
		.enumerate()
		.collect()
		.map(|mut pdus: Vec<_>| {
			pdus.sort_by(|(_, (room_a, ..)), (_, (room_b, ..))| room_a.cmp(room_b));
			pdus.into_iter()
				.into_grouping_map_by(|(_, (room_id, ..))| room_id.clone())
				.collect()
		})
		.await;

	// we can evaluate rooms concurrently
	let results: ResolvedMap = pdus
		.into_iter()
		.try_stream()
		.broad_and_then(async |(room_id, pdus): (_, Vec<_>)| {
			handle_room(services, client, origin, &started, room_id, pdus.into_iter())
				.map_ok(ResolvedMap::into_iter)
				.map_ok(IterStream::try_stream)
				.await
		})
		.try_flatten()
		.try_collect()
		.await?;

	// evaluate edus after pdus, at least for now.
	edus.enumerate()
		.for_each_concurrent(automatic_width(), |(i, edu)| {
			handle_edu(services, client, origin, i, edu)
		})
		.await;

	Ok(results)
}

async fn handle_room(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	txn_start_time: &Instant,
	ref room_id: OwnedRoomId,
	pdus: impl Iterator<Item = (usize, Pdu)> + Send,
) -> Result<ResolvedMap> {
	let _room_lock = services
		.event_handler
		.mutex_federation
		.lock(room_id)
		.await;

	pdus.enumerate()
		.try_stream()
		.and_then(async |(ri, (ti, (room_id, event_id, value)))| {
			services.server.check_running()?;
			let pdu_start_time = Instant::now();
			let result = services
				.event_handler
				.handle_incoming_pdu(origin, &room_id, &event_id, value, true)
				.map_ok(|_| ())
				.await;

			debug!(
				%event_id,
				%room_id,
				ri,
				ti,
				pdu_elapsed = ?pdu_start_time.elapsed(),
				txn_elapsed = ?txn_start_time.elapsed(),
				"Finished PDU",
			);

			Ok((event_id, result))
		})
		.try_collect()
		.boxed()
		.await
}

async fn handle_edu(
	services: &Services,
	client: &IpAddr,
	origin: &ServerName,
	i: usize,
	edu: Edu,
) {
	match edu {
		| Edu::Presence(presence) if services.server.config.allow_incoming_presence =>
			handle_edu_presence(services, client, origin, presence).await,

		| Edu::Receipt(receipt)
			if services
				.server
				.config
				.allow_incoming_read_receipts =>
			handle_edu_receipt(services, client, origin, receipt).await,

		| Edu::Typing(typing) if services.server.config.allow_incoming_typing =>
			handle_edu_typing(services, client, origin, typing).await,

		| Edu::DeviceListUpdate(content) =>
			handle_edu_device_list_update(services, client, origin, content).await,

		| Edu::DirectToDevice(content) =>
			handle_edu_direct_to_device(services, client, origin, content).await,

		| Edu::SigningKeyUpdate(content) =>
			handle_edu_signing_key_update(services, client, origin, content).await,

		| Edu::_Custom(ref _custom) => debug_warn!(?i, ?edu, "received custom/unknown EDU"),

		| _ => trace!(?i, ?edu, "skipped"),
	}
}

async fn handle_edu_presence(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	presence: PresenceContent,
) {
	presence
		.push
		.into_iter()
		.stream()
		.for_each_concurrent(automatic_width(), |update| {
			handle_edu_presence_update(services, origin, update)
		})
		.await;
}

async fn handle_edu_presence_update(
	services: &Services,
	origin: &ServerName,
	update: PresenceUpdate,
) {
	if update.user_id.server_name() != origin {
		debug_warn!(
			%update.user_id, %origin,
			"received presence EDU for user not belonging to origin"
		);
		return;
	}

	services
		.presence
		.set_presence_from_federation(
			&update.user_id,
			&update.presence,
			update.currently_active,
			update.last_active_ago,
			update.status_msg.clone(),
		)
		.await
		.log_err()
		.ok();
}

async fn handle_edu_receipt(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	receipt: ReceiptContent,
) {
	receipt
		.receipts
		.into_iter()
		.stream()
		.for_each_concurrent(automatic_width(), |(room_id, room_updates)| {
			handle_edu_receipt_room(services, origin, room_id, room_updates)
		})
		.await;
}

async fn handle_edu_receipt_room(
	services: &Services,
	origin: &ServerName,
	room_id: OwnedRoomId,
	room_updates: ReceiptMap,
) {
	if services
		.event_handler
		.acl_check(origin, &room_id)
		.await
		.is_err()
	{
		debug_warn!(
			%origin, %room_id,
			"received read receipt EDU from ACL'd server"
		);
		return;
	}

	let room_id = &room_id;
	room_updates
		.read
		.into_iter()
		.stream()
		.for_each_concurrent(automatic_width(), async |(user_id, user_updates)| {
			handle_edu_receipt_room_user(services, origin, room_id, &user_id, user_updates).await;
		})
		.await;
}

async fn handle_edu_receipt_room_user(
	services: &Services,
	origin: &ServerName,
	room_id: &RoomId,
	user_id: &UserId,
	user_updates: ReceiptData,
) {
	if user_id.server_name() != origin {
		debug_warn!(
			%user_id, %origin,
			"received read receipt EDU for user not belonging to origin"
		);
		return;
	}

	if !services
		.state_cache
		.server_in_room(origin, room_id)
		.await
	{
		debug_warn!(
			%user_id, %room_id, %origin,
			"received read receipt EDU from server who does not have a member in the room",
		);
		return;
	}

	let data = &user_updates.data;
	user_updates
		.event_ids
		.into_iter()
		.stream()
		.for_each_concurrent(automatic_width(), async |event_id| {
			let user_data = [(user_id.to_owned(), data.clone())];
			let receipts = [(ReceiptType::Read, BTreeMap::from(user_data))];
			let content = [(event_id.clone(), BTreeMap::from(receipts))];
			services
				.read_receipt
				.readreceipt_update(user_id, room_id, &ReceiptEvent {
					content: ReceiptEventContent(content.into()),
					room_id: room_id.to_owned(),
				})
				.await;
		})
		.await;
}

async fn handle_edu_typing(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	typing: TypingContent,
) {
	if typing.user_id.server_name() != origin {
		debug_warn!(
			%typing.user_id, %origin,
			"received typing EDU for user not belonging to origin"
		);
		return;
	}

	if services
		.event_handler
		.acl_check(typing.user_id.server_name(), &typing.room_id)
		.await
		.is_err()
	{
		debug_warn!(
			%typing.user_id, %typing.room_id, %origin,
			"received typing EDU for ACL'd user's server"
		);
		return;
	}

	if !services
		.state_cache
		.is_joined(&typing.user_id, &typing.room_id)
		.await
	{
		debug_warn!(
			%typing.user_id, %typing.room_id, %origin,
			"received typing EDU for user not in room"
		);
		return;
	}

	if typing.typing {
		let secs = services.server.config.typing_federation_timeout_s;
		let timeout = millis_since_unix_epoch().saturating_add(secs.saturating_mul(1000));

		services
			.typing
			.typing_add(&typing.user_id, &typing.room_id, timeout)
			.await
			.log_err()
			.ok();
	} else {
		services
			.typing
			.typing_remove(&typing.user_id, &typing.room_id)
			.await
			.log_err()
			.ok();
	}
}

async fn handle_edu_device_list_update(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	content: DeviceListUpdateContent,
) {
	let DeviceListUpdateContent { user_id, .. } = content;

	if user_id.server_name() != origin {
		debug_warn!(
			%user_id, %origin,
			"received device list update EDU for user not belonging to origin"
		);
		return;
	}

	services
		.users
		.mark_device_key_update(&user_id)
		.await;
}

async fn handle_edu_direct_to_device(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	content: DirectDeviceContent,
) {
	let DirectDeviceContent {
		ref sender,
		ref ev_type,
		ref message_id,
		messages,
	} = content;

	if sender.server_name() != origin {
		debug_warn!(
			%sender, %origin,
			"received direct to device EDU for user not belonging to origin"
		);
		return;
	}

	// Check if this is a new transaction id
	if services
		.transaction_ids
		.existing_txnid(sender, None, message_id)
		.await
		.is_ok()
	{
		return;
	}

	// process messages concurrently for different users
	let ev_type = ev_type.to_string();
	messages
		.into_iter()
		.stream()
		.for_each_concurrent(automatic_width(), |(target_user_id, map)| {
			handle_edu_direct_to_device_user(services, target_user_id, sender, &ev_type, map)
		})
		.await;

	// Save transaction id with empty data
	services
		.transaction_ids
		.add_txnid(sender, None, message_id, &[]);
}

async fn handle_edu_direct_to_device_user<Event: Send + Sync>(
	services: &Services,
	target_user_id: OwnedUserId,
	sender: &UserId,
	ev_type: &str,
	map: BTreeMap<DeviceIdOrAllDevices, Raw<Event>>,
) {
	for (target_device_id_maybe, event) in map {
		let Ok(event) = event
			.deserialize_as()
			.map_err(|e| err!(Request(InvalidParam(error!("To-Device event is invalid: {e}")))))
		else {
			continue;
		};

		handle_edu_direct_to_device_event(
			services,
			&target_user_id,
			sender,
			target_device_id_maybe,
			ev_type,
			event,
		)
		.await;
	}
}

async fn handle_edu_direct_to_device_event(
	services: &Services,
	target_user_id: &UserId,
	sender: &UserId,
	target_device_id_maybe: DeviceIdOrAllDevices,
	ev_type: &str,
	event: serde_json::Value,
) {
	match target_device_id_maybe {
		| DeviceIdOrAllDevices::DeviceId(ref target_device_id) => {
			services.users.add_to_device_event(
				sender,
				target_user_id,
				target_device_id,
				ev_type,
				&event,
			);
		},

		| DeviceIdOrAllDevices::AllDevices => {
			services
				.users
				.all_device_ids(target_user_id)
				.ready_for_each(|target_device_id| {
					services.users.add_to_device_event(
						sender,
						target_user_id,
						target_device_id,
						ev_type,
						&event,
					);
				})
				.await;
		},
	}
}

async fn handle_edu_signing_key_update(
	services: &Services,
	_client: &IpAddr,
	origin: &ServerName,
	content: SigningKeyUpdateContent,
) {
	let SigningKeyUpdateContent { user_id, master_key, self_signing_key } = content;

	if user_id.server_name() != origin {
		debug_warn!(
			%user_id, %origin,
			"received signing key update EDU from server that does not belong to user's server"
		);
		return;
	}

	services
		.users
		.add_cross_signing_keys(&user_id, &master_key, &self_signing_key, &None, true)
		.await
		.log_err()
		.ok();
}
