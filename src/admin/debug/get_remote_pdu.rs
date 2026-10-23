use futures::FutureExt;
use ruma::{CanonicalJsonObject, OwnedEventId, OwnedServerName};
use serde_json::value::RawValue as RawJsonValue;
use tuwunel_core::{Err, Result, err, info, warn};
use tuwunel_service::fetcher::{Op, Opts};

use crate::admin_command;

#[admin_command]
pub(super) async fn get_remote_pdu(
	&self,
	event_id: OwnedEventId,
	server: OwnedServerName,
) -> Result {
	self.services.server.config.require_federation()?;

	if server == self.services.globals.server_name() {
		return Err!(
			"Not allowed to send federation requests to ourselves. Please use `get-pdu` for \
			 fetching local PDUs.",
		);
	}

	let opts = Opts::unscoped(Op::Event)
		.event_id(event_id.clone())
		.hint(server.clone())
		.checks(false);

	let outcome = self
		.services
		.fetcher
		.fetch(opts)
		.await
		.map_err(|e| {
			err!("Remote server did not have PDU or failed sending request to remote server: {e}")
		})?;

	let pdu: Box<RawJsonValue> = serde_json::from_slice(&outcome.bytes).map_err(|e| {
		warn!(
			"Requested event ID {event_id} from {server} but failed to parse the response as a \
			 PDU (malformed event/response?): {e}"
		);
		err!(Request(Unknown("Received response from server but failed to parse PDU")))
	})?;

	let json: CanonicalJsonObject = serde_json::from_slice(&outcome.bytes).map_err(|e| {
		err!(
			"Got a PDU from {server} but could not convert it to canonical JSON for display: {e}"
		)
	})?;

	let (room_id, ..) = self
		.services
		.event_handler
		.parse_incoming_pdu(&pdu)
		.boxed()
		.await
		.map_err(|e| {
			warn!("Failed to parse PDU: {e}");
			info!("Full PDU: {pdu:?}");
			err!("Failed to parse PDU remote server {server} sent us: {e}")
		})?;

	info!("Attempting to handle event ID {event_id} as backfilled PDU");
	self.services
		.timeline
		.backfill_pdu(&room_id, &outcome.origin, pdu)
		.await?;

	let text = serde_json::to_string_pretty(&json)?;
	let msg = "Got PDU from specified server and handled as backfilled";
	write!(self, "{msg}. Event body:\n```json\n{text}\n```").await
}
