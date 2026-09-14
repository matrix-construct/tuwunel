use ruma::api::federation::transactions::send_transaction_message::v1::Request;

use super::*;

impl Service {
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
			.filter_map(|edu| match edu {
				| SendingEvent::Edu(edu) => Some(edu.as_ref()),
				| _ => None,
			})
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

		for (event_id, result) in result.iter().flat_map(|resp| resp.pdus.iter()) {
			if let Err(e) = result {
				warn!(
					%txn_id, %server,
					"error sending PDU {event_id} to remote server: {e:?}"
				);
			}
		}

		match result {
			| Ok(_) => Ok(Destination::Federation(server)),
			| Err(error) => Err((Destination::Federation(server), error)),
		}
	}
}
