//! Wire types for the MSC4521 federation endpoints.

use ruma::{
	OwnedEventId, OwnedRoomId,
	api::{auth_scheme::ServerSignatures, metadata, request, response},
};

pub(crate) mod get_room_digest {
	use super::*;

	metadata! {
		method: GET,
		rate_limited: false,
		authentication: ServerSignatures,
		history: {
			unstable => "/_matrix/federation/unstable/tk.nutra.msc0501/room_digest/{room_id}",
		},
	}

	#[request]
	pub struct Request {
		#[ruma_api(path)]
		pub room_id: OwnedRoomId,
	}

	#[response]
	pub struct Response {
		pub digest: String,
		pub digest_type: String,
		pub known_event_count: u64,
		pub frame_id: String,
		pub extremity_event_ids: Vec<OwnedEventId>,
		pub frame_event_ids: Vec<OwnedEventId>,
		pub depth_range: Vec<u64>,
		pub origin_server_ts_range: Vec<u64>,
	}
}

pub(crate) mod get_room_digest_strata {
	use super::*;

	metadata! {
		method: GET,
		rate_limited: false,
		authentication: ServerSignatures,
		history: {
			unstable => "/_matrix/federation/unstable/tk.nutra.msc0501/room_digest/{room_id}/strata",
		},
	}

	#[request]
	pub struct Request {
		#[ruma_api(path)]
		pub room_id: OwnedRoomId,
	}

	#[response]
	pub struct Response {
		pub digest: String,
		pub digest_type: String,
		pub known_event_count: u64,
		pub frame_id: String,
		pub extremity_event_ids: Vec<OwnedEventId>,
		pub frame_event_ids: Vec<OwnedEventId>,
		pub depth_range: Vec<u64>,
		pub origin_server_ts_range: Vec<u64>,
		pub strata: Vec<String>,
	}
}

pub(crate) mod post_room_diff {
	use super::*;

	#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
	pub struct SketchBucket {
		pub depth: u64,
		pub prefix: u64,
		pub capacity: u64,
	}

	metadata! {
		method: POST,
		rate_limited: false,
		authentication: ServerSignatures,
		history: {
			unstable => "/_matrix/federation/unstable/tk.nutra.msc0501/room_diff/{room_id}",
		},
	}

	#[request]
	pub struct Request {
		#[ruma_api(path)]
		pub room_id: OwnedRoomId,
		pub mode: String,
		pub digest_type: String,
		pub local_digest: String,
		pub local_known_event_count: u64,
		pub frame_id: String,
		pub frame_event_ids: Vec<OwnedEventId>,
		pub frame_negotiation: bool,
		pub scope: Option<String>,
		pub state_at: Option<OwnedEventId>,
		pub requests: Vec<SketchBucket>,
		pub local_sketches: Vec<String>,
	}

	#[response]
	pub struct Response {
		pub digest_type: String,
		pub missing_event_ids: Vec<OwnedEventId>,
		pub requester_only_short_ids: Option<Vec<String>>,
		pub expected_requester_side_accumulator: Option<String>,
		pub remote_known_event_count: u64,
		pub remote_extremity_event_ids: Vec<OwnedEventId>,
		pub frame_id: String,
		pub frame_status: String,
		pub negotiated_frame_event_ids: Option<Vec<OwnedEventId>>,
		pub scope: Option<String>,
		pub state_at: Option<OwnedEventId>,
		pub sketch_status: String,
		pub truncated: bool,
	}
}
