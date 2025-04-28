mod acl_check;
mod fetch_and_handle_outliers;
mod fetch_prev;
mod fetch_state;
mod handle_incoming_pdu;
mod handle_outlier_pdu;
mod handle_prev_pdu;
mod parse_incoming_pdu;
mod resolve_state;
mod state_at_incoming;
mod upgrade_outlier_pdu;

use std::{
	collections::HashMap,
	fmt::Write,
	sync::{Arc, RwLock as StdRwLock},
	time::Instant,
};

use async_trait::async_trait;
use ruma::{
	OwnedEventId, OwnedRoomId, RoomId, RoomVersionId,
	events::room::create::RoomCreateEventContent,
};
use tuwunel_core::{Err, PduEvent, Result, RoomVersion, Server, utils::MutexMap};

use crate::{Dep, globals, rooms, sending, server_keys};

pub struct Service {
	pub mutex_federation: RoomMutexMap,
	pub federation_handletime: StdRwLock<HandleTimeMap>,
	services: Services,
}

struct Services {
	globals: Dep<globals::Service>,
	sending: Dep<sending::Service>,
	auth_chain: Dep<rooms::auth_chain::Service>,
	metadata: Dep<rooms::metadata::Service>,
	outlier: Dep<rooms::outlier::Service>,
	pdu_metadata: Dep<rooms::pdu_metadata::Service>,
	server_keys: Dep<server_keys::Service>,
	short: Dep<rooms::short::Service>,
	state: Dep<rooms::state::Service>,
	state_accessor: Dep<rooms::state_accessor::Service>,
	state_compressor: Dep<rooms::state_compressor::Service>,
	timeline: Dep<rooms::timeline::Service>,
	server: Arc<Server>,
}

type RoomMutexMap = MutexMap<OwnedRoomId, ()>;
type HandleTimeMap = HashMap<OwnedRoomId, (OwnedEventId, Instant)>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			mutex_federation: RoomMutexMap::new(),
			federation_handletime: HandleTimeMap::new().into(),
			services: Services {
				globals: args.depend::<globals::Service>("globals"),
				sending: args.depend::<sending::Service>("sending"),
				auth_chain: args.depend::<rooms::auth_chain::Service>("rooms::auth_chain"),
				metadata: args.depend::<rooms::metadata::Service>("rooms::metadata"),
				outlier: args.depend::<rooms::outlier::Service>("rooms::outlier"),
				server_keys: args.depend::<server_keys::Service>("server_keys"),
				pdu_metadata: args.depend::<rooms::pdu_metadata::Service>("rooms::pdu_metadata"),
				short: args.depend::<rooms::short::Service>("rooms::short"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				state_accessor: args
					.depend::<rooms::state_accessor::Service>("rooms::state_accessor"),
				state_compressor: args
					.depend::<rooms::state_compressor::Service>("rooms::state_compressor"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
				server: args.server.clone(),
			},
		}))
	}

	async fn memory_usage(&self, out: &mut (dyn Write + Send)) -> Result {
		let mutex_federation = self.mutex_federation.len();
		writeln!(out, "federation_mutex: {mutex_federation}")?;

		let federation_handletime = self
			.federation_handletime
			.read()
			.expect("locked for reading")
			.len();
		writeln!(out, "federation_handletime: {federation_handletime}")?;

		Ok(())
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

impl Service {
	async fn event_exists(&self, event_id: OwnedEventId) -> bool {
		self.services.timeline.pdu_exists(&event_id).await
	}

	async fn event_fetch(&self, event_id: OwnedEventId) -> Option<PduEvent> {
		self.services
			.timeline
			.get_pdu(&event_id)
			.await
			.ok()
	}
}

fn check_room_id(room_id: &RoomId, pdu: &PduEvent) -> Result {
	if pdu.room_id != room_id {
		return Err!(Request(InvalidParam(error!(
			pdu_event_id = ?pdu.event_id,
			pdu_room_id = ?pdu.room_id,
			?room_id,
			"Found event from room in room",
		))));
	}

	Ok(())
}

fn get_room_version_id(create_event: &PduEvent) -> Result<RoomVersionId> {
	let content: RoomCreateEventContent = create_event.get_content()?;
	let room_version = content.room_version;

	Ok(room_version)
}

#[inline]
fn to_room_version(room_version_id: &RoomVersionId) -> RoomVersion {
	RoomVersion::new(room_version_id).expect("room version is supported")
}
