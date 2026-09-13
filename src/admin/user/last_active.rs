use std::cmp::{self, Reverse};

use futures::{FutureExt, StreamExt, TryStreamExt};
use ruma::{MilliSecondsSinceUnixEpoch, uint};
use tuwunel_core::{
	Result,
	itertools::Itertools,
	utils::{ReadyExt, stream::IterStream},
};

use crate::admin_command;

#[admin_command]
pub(super) async fn last_active(&self, limit: Option<usize>) -> Result {
	self.services
		.users
		.list_local_users()
		.map(ToOwned::to_owned)
		.then(async |user_id| {
			self.services
				.users
				.all_devices_metadata(&user_id)
				.ready_filter_map(|device| {
					device
						.last_seen_ts
						.map(|ts| (ts, device.last_seen_ip))
				})
				.ready_fold((MilliSecondsSinceUnixEpoch(uint!(0)), None), cmp::max)
				.map(|(last_seen_ts, last_seen_ip)| (last_seen_ts, last_seen_ip, user_id.clone()))
				.await
		})
		.ready_filter(|(ts, ..)| ts.get() > uint!(0))
		.collect::<Vec<_>>()
		.map(IntoIterator::into_iter)
		.map(|iter| iter.sorted_by_key(|k| Reverse(k.0)))
		.map(IterStream::try_stream)
		.flatten_stream()
		.take(limit.unwrap_or(48))
		.try_for_each(async |(last_seen_ts, last_seen_ip, user_id)| {
			let ago = last_seen_ts;
			let user_id = user_id.localpart();
			let ip = last_seen_ip.as_deref().unwrap_or_default();

			write!(self, "{ago:?} {ip:<40} {user_id}\n").await
		})
		.boxed()
		.await
}
