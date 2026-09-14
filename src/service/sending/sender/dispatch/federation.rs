use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::StreamExt;
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedServerName,
	api::federation::transactions::{edu::Edu, send_transaction_message::v1::Request},
	serde::Raw,
};
use tuwunel_core::{
	extract_variant, implement,
	utils::{IterStream, calculate_hash, future::TryExtExt, stream::WidebandExt},
	warn,
};

use super::SendingResult;
use crate::sending::{Destination, EduBuf, SendingEvent, Service};

#[implement(Service)]
#[tracing::instrument(
	name = "federation",
	level = "debug",
	skip(self, events),
	fields(
		events = %events.len(),
	),
)]
pub(super) async fn send_events_dest_federation(
	&self,
	server: OwnedServerName,
	events: Vec<SendingEvent>,
) -> SendingResult {
	let pdus: Vec<_> = events
		.iter()
		.filter_map(|event| extract_variant!(event, SendingEvent::Pdu))
		.stream()
		.wide_filter_map(|pdu_id| {
			self.services
				.timeline
				.get_pdu_json_from_id(pdu_id)
				.ok()
		})
		.wide_then(|pdu| {
			self.services
				.state_accessor
				.erased_for_server(&server, pdu)
		})
		.wide_then(|pdu| {
			self.services
				.federation
				.format_pdu_into(pdu, None)
		})
		.collect()
		.await;

	let edus: Vec<Raw<Edu>> = events
		.iter()
		.filter_map(|event| extract_variant!(event, SendingEvent::Edu))
		.map(EduBuf::as_slice)
		.map(serde_json::from_slice)
		.filter_map(Result::ok)
		.collect();

	if pdus.is_empty() && edus.is_empty() {
		return Ok(Destination::Federation(server));
	}

	let preimage = pdus
		.iter()
		.map(|raw| raw.get().as_bytes())
		.chain(edus.iter().map(|raw| raw.json().get().as_bytes()));

	let txn_hash = calculate_hash(preimage);
	let txn_id = &*URL_SAFE_NO_PAD.encode(txn_hash);
	let request = Request {
		transaction_id: txn_id.into(),
		origin: self.server.name.clone(),
		origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
		pdus,
		edus,
	};

	let result = self
		.services
		.federation
		.execute_on(&self.services.client.sender, &server, request)
		.await;

	result
		.iter()
		.flat_map(|resp| resp.pdus.iter())
		.filter_map(|(event_id, result)| {
			result
				.as_ref()
				.err()
				.map(|error| (event_id, error))
		})
		.for_each(|(event_id, error)| {
			warn!(%txn_id, %server, %event_id, %error, "error sending PDU to remote server");
		});

	match result {
		| Ok(_) => Ok(Destination::Federation(server)),
		| Err(error) => Err((Destination::Federation(server), error)),
	}
}
