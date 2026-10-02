use std::collections::BTreeMap;

use futures::StreamExt;
use tuwunel_core::{Result, utils::ReadyExt};

use crate::admin_command;

#[admin_command]
pub(super) async fn sending_parked_rooms(&self) -> Result {
	let query = async {
		let parks: Vec<_> = self.services.sending.db.parked().collect().await;
		let rooms: BTreeMap<_, _> = self
			.services
			.short
			.iter_shortroomids()
			.ready_filter_map(|(room_id, short)| {
				let parked = parks.iter().any(|(_, park)| park.room == short);

				parked.then(|| (short, room_id.to_owned()))
			})
			.collect()
			.await;

		parks
			.into_iter()
			.map(|(server, park)| (server, rooms.get(&park.room).cloned(), park))
			.collect::<Vec<_>>()
	};

	self.write_timed_query(query).await
}
