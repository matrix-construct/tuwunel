use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	task::Wake,
};

use serde_json::Value;
use tuwunel_core::{implement, matrix::pdu::RawPduId};

use crate::Services;

pub(super) struct Observation {
	services: Arc<Services>,
	root_id: RawPduId,
	reply_id: RawPduId,
	notified: AtomicBool,
	consistent: AtomicBool,
}

impl Observation {
	pub(super) fn new(services: &Arc<Services>, root_id: RawPduId, reply_id: RawPduId) -> Self {
		Self {
			services: services.clone(),
			root_id,
			reply_id,
			notified: AtomicBool::new(false),
			consistent: AtomicBool::new(true),
		}
	}
}

#[implement(Observation)]
#[inline]
pub(super) fn was_consistent(&self) -> bool {
	self.notified.load(Ordering::SeqCst) && self.consistent.load(Ordering::SeqCst)
}

impl Wake for Observation {
	fn wake(self: Arc<Self>) { self.wake_by_ref(); }

	fn wake_by_ref(self: &Arc<Self>) {
		self.consistent
			.fetch_and(self.matches(), Ordering::SeqCst);

		self.notified.store(true, Ordering::SeqCst);
	}
}

#[implement(Observation)]
fn matches(&self) -> bool {
	self.services.db["threadactivityid_rootid"]
		.get_blocking(&self.reply_id)
		.is_ok_and(|value| &*value == self.root_id.as_bytes())
		&& self.services.db["threadrootid_latestcount"]
			.get_blocking(&self.root_id)
			.is_ok_and(|value| *value == self.reply_id.pdu_count().to_be_bytes())
		&& self.services.db["pduid_pdu"]
			.get_blocking(&self.root_id)
			.is_ok_and(|value| {
				serde_json::from_slice(&value).is_ok_and(|pdu: Value| {
					let thread = &pdu["unsigned"]["m.relations"]["m.thread"];

					thread["count"] == 1
						&& thread["latest_event"]["event_id"] == "$reply:localhost"
						&& thread["latest_event"]["sender"] == "@reply:localhost"
				})
			})
}
