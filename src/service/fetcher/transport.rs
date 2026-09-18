//! Federation transport: an [`Op`] and target server in, raw response bytes
//! out.
//!
//! The [`Transport`] seam isolates the network behind a trait the tests mock;
//! [`FederationTransport`] is the production impl.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, ServerName, UInt,
	api::federation::{
		authorization::get_event_authorization::v1::Request as EventAuthRequest,
		backfill::get_backfill::v1::Request as BackfillRequest,
		event::{
			get_event::v1::Request as EventRequest,
			get_event_by_timestamp::v1::Request as TimestampRequest,
			get_missing_events::v1::Request as MissingEventsRequest,
			get_room_state_ids::v1::Request as StateIdsRequest,
		},
	},
};
use tuwunel_core::{Result, err, utils::BoolExt};

use super::{Op, Opts};
use crate::services::OnceServices;

/// Abstracts the network operation for one federation fetch attempt.
///
/// The production implementation routes through federation execution while
/// tests substitute a scripted mock.
#[async_trait]
pub(super) trait Transport: Send + Sync {
	/// Executes one endpoint operation and returns its raw response body.
	///
	/// Required option fields are validated before the federation request is sent.
	async fn fetch_raw(&self, op: Op, server: &ServerName, opts: &Opts) -> Result<Bytes>;
}

/// Production transport backed by the federation request service.
///
/// Each operation is converted to its corresponding ruma federation request.
pub(super) struct FederationTransport {
	/// Services used to execute typed federation requests.
	pub(super) services: Arc<OnceServices>,
}

#[async_trait]
impl Transport for FederationTransport {
	#[tracing::instrument(
		level = "debug",
		skip(self, opts),
		fields(
			%server,
		),
	)]
	async fn fetch_raw(&self, op: Op, server: &ServerName, opts: &Opts) -> Result<Bytes> {
		let federation = &self.services.federation;

		match op {
			| Op::Event | Op::AuthEvent => {
				let event_id = require_event_id(opts)?;
				let res = federation
					.execute(server, EventRequest { event_id })
					.await?;

				Ok(Bytes::copy_from_slice(res.pdu.get().as_bytes()))
			},
			| Op::AuthChain => {
				let event_id = require_event_id(opts)?;
				let room_id = require_room_id(opts)?;
				let res = federation
					.execute(server, EventAuthRequest { room_id, event_id })
					.await?;

				to_bytes(&res.auth_chain)
			},
			| Op::Backfill => {
				let event_id = require_event_id(opts)?;
				let room_id = require_room_id(opts)?;
				let res = federation
					.execute(server, BackfillRequest {
						room_id,
						v: vec![event_id],
						limit: batch_limit(opts),
					})
					.await?;

				to_bytes(&res.pdus)
			},
			| Op::StateIds => {
				let event_id = require_event_id(opts)?;
				let room_id = require_room_id(opts)?;
				let res = federation
					.execute(server, StateIdsRequest { room_id, event_id })
					.await?;

				to_bytes(&serde_json::json!({
					"auth_chain_ids": res.auth_chain_ids,
					"pdu_ids": res.pdu_ids,
				}))
			},
			| Op::MissingEvents => {
				require_latest_events(opts)?;
				let room_id = require_room_id(opts)?;
				let req = MissingEventsRequest {
					room_id,
					earliest_events: opts.earliest_events.to_vec(),
					latest_events: opts.latest_events.to_vec(),
					limit: batch_limit(opts),
					min_depth: UInt::default(),
				};

				let res = federation.execute(server, req).await?;

				to_bytes(&res.events)
			},
			| Op::TimestampToEvent => {
				let room_id = require_room_id(opts)?;
				let ts = require_ts(opts)?;
				let res = federation
					.execute(
						server,
						TimestampRequest::new(room_id, ts, opts.dir.unwrap_or_default()),
					)
					.await?;

				to_bytes(&serde_json::json!({
					"event_id": res.event_id,
					"origin_server_ts": res.origin_server_ts,
				}))
			},
		}
	}
}

fn require_event_id(opts: &Opts) -> Result<OwnedEventId> {
	opts.event_id
		.clone()
		.ok_or_else(|| err!(Request(InvalidParam("event_id is required for op {:?}", opts.op))))
}

fn require_room_id(opts: &Opts) -> Result<OwnedRoomId> {
	opts.room_id
		.clone()
		.ok_or_else(|| err!(Request(InvalidParam("room_id is required for op {:?}", opts.op))))
}

fn require_ts(opts: &Opts) -> Result<MilliSecondsSinceUnixEpoch> {
	opts.ts
		.ok_or_else(|| err!(Request(InvalidParam("ts is required for op {:?}", opts.op))))
}

fn require_latest_events(opts: &Opts) -> Result {
	opts.latest_events
		.is_empty()
		.is_false()
		.then_some(())
		.ok_or_else(|| {
			err!(Request(InvalidParam("latest_events is required for op {:?}", opts.op)))
		})
}

/// Event count requested per batch op, defaulting to the federation default of
/// 10 and saturating an oversized cap to the wire `UInt`.
fn batch_limit(opts: &Opts) -> UInt {
	opts.backfill_limit.map_or_else(
		|| UInt::from(10_u8),
		|n| UInt::new_saturating(u64::try_from(n.get()).unwrap_or(u64::MAX)),
	)
}

fn to_bytes<T: serde::Serialize>(value: &T) -> Result<Bytes> {
	serde_json::to_vec(value)
		.map(Bytes::from)
		.map_err(|e| err!(BadServerResponse("failed to re-encode federation response: {e}")))
}
