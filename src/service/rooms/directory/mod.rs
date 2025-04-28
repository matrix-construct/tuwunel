use std::sync::Arc;

use futures::Stream;
use ruma::{RoomId, api::client::room::Visibility};
use tuwunel_core::{Result, implement, utils::stream::TryIgnore};
use tuwunel_database::Map;

pub struct Service {
	db: Data,
}

struct Data {
	publicroomids: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			db: Data {
				publicroomids: args.db["publicroomids"].clone(),
			},
		}))
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}
}

#[implement(Service)]
pub fn set_public(&self, room_id: &RoomId) {
	self.db.publicroomids.insert(room_id, []);
}

#[implement(Service)]
pub fn set_not_public(&self, room_id: &RoomId) {
	self.db.publicroomids.remove(room_id);
}

#[implement(Service)]
pub fn public_rooms(&self) -> impl Stream<Item = &RoomId> + Send {
	self.db.publicroomids.keys().ignore_err()
}

#[implement(Service)]
pub async fn is_public_room(&self, room_id: &RoomId) -> bool {
	self.visibility(room_id).await == Visibility::Public
}

#[implement(Service)]
pub async fn visibility(&self, room_id: &RoomId) -> Visibility {
	if self.db.publicroomids.get(room_id).await.is_ok() {
		Visibility::Public
	} else {
		Visibility::Private
	}
}
