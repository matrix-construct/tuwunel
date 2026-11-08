mod account_data;
mod e2ee;
mod profiles;
mod receipts;
mod to_device;
mod typing;

#[cfg(test)]
mod tests;

use std::{collections::BTreeMap, fmt::Debug};

use futures::{FutureExt, future::join4};
use ruma::{
	OwnedRoomId, RoomId,
	api::client::sync::sync_events::v5::{
		ListId,
		request::ExtensionRoomConfig,
		response::{Extensions, Room as ResponseRoom},
	},
};
use tuwunel_core::{Result, apply, at, extract_variant, implement, utils::BoolExt};
use tuwunel_service::sync::Connection;

pub(super) use self::to_device::{
	acknowledge as acknowledge_to_device, check as check_to_device, pending as to_device_pending,
};
use self::{
	account_data::{
		collect as collect_account_data, collect_ranges as collect_account_data_ranges,
	},
	profiles::collect as collect_profiles,
	receipts::collect_ranges as collect_receipt_ranges,
};
use super::{SyncInfo, Window, WindowRoom, joined_since, range::Results, share_encrypted_room};

pub(super) struct Collected {
	response: Extensions,
	typing: typing::Collected,
	/// Whether the one-time-key counts changed since the connection's position.
	keys_changed: bool,
}

#[tracing::instrument(
	name = "extensions",
	level = "debug",
	skip_all,
	fields(
		next_batch = conn.next_batch,
		window = window.len(),
		rooms = conn.rooms.len(),
		subs = conn.subscriptions.len(),
	)
)]
pub(super) async fn handle(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
) -> Result<Collected> {
	let account_data = conn
		.extensions
		.account_data
		.enabled
		.unwrap_or(false)
		.then_async(|| collect_account_data(sync_info, conn));

	let typing = conn
		.extensions
		.typing
		.enabled
		.unwrap_or(false)
		.then_async(|| typing::collect(sync_info, conn, window));

	let to_device = conn
		.extensions
		.to_device
		.enabled
		.unwrap_or(false)
		.then_async(|| to_device::collect(sync_info, conn));

	let e2ee = conn
		.extensions
		.e2ee
		.enabled
		.unwrap_or(false)
		.then_async(|| e2ee::collect(sync_info, conn));

	let (account_data, typing, to_device, e2ee) = join4(account_data, typing, to_device, e2ee)
		.map(apply!(4, |t: Option<_>| t.unwrap_or(Ok(Default::default()))))
		.await;

	let (e2ee, keys_changed) = e2ee?;

	// Receipt and room account-data payloads only exist as bounded room-range
	// outputs, applied by `apply_ranges` after the ranges resolve.
	let response = Extensions {
		account_data: account_data?,
		receipts: Default::default(),
		typing: Default::default(),
		to_device: to_device?,
		e2ee,
		profiles: Default::default(),
	};

	Ok(Collected { response, typing: typing?, keys_changed })
}

#[implement(Collected)]
#[inline]
pub(super) fn keys_changed(&self) -> bool { self.keys_changed }

#[implement(Collected)]
pub(super) fn into_response(
	mut self,
	payloads: &BTreeMap<OwnedRoomId, ResponseRoom>,
) -> Extensions {
	self.response.typing = self.typing.into_response(payloads);
	self.response
}

#[tracing::instrument(level = "trace", skip_all)]
pub(super) async fn apply_profiles(
	sync_info: SyncInfo<'_>,
	conn: &Connection,
	window: &Window,
	ranges: &Results,
	extensions: &mut Collected,
) -> Result {
	if conn.extensions.profiles.enabled.unwrap_or(false) {
		extensions.response.profiles = collect_profiles(sync_info, conn, window, ranges).await?;
	}

	Ok(())
}

pub(super) fn apply_ranges(
	conn: &Connection,
	window: &Window,
	ranges: &mut Results,
	extensions: &mut Collected,
) {
	if conn.extensions.receipts.enabled.unwrap_or(false) {
		extensions.response.receipts = collect_receipt_ranges(conn, window, ranges);
	}

	if conn
		.extensions
		.account_data
		.enabled
		.unwrap_or(false)
	{
		extensions.response.account_data.rooms =
			collect_account_data_ranges(conn, window, ranges);
	}
}

#[tracing::instrument(
	name = "selector",
	level = "trace",
	skip_all,
	fields(?implicit, ?explicit),
)]
fn selector<'a, ListIter, SubsIter>(
	conn: &'a Connection,
	window: &'a Window,
	implicit: Option<ListIter>,
	explicit: Option<SubsIter>,
) -> impl Iterator<Item = &'a RoomId> + Send + Sync + 'a
where
	ListIter: Iterator<Item = &'a ListId> + Clone + Debug + Send + Sync + 'a,
	SubsIter: Iterator<Item = &'a ExtensionRoomConfig> + Clone + Debug + Send + Sync + 'a,
{
	let has_all_subscribed = explicit
		.clone()
		.into_iter()
		.flatten()
		.any(|erc| matches!(erc, ExtensionRoomConfig::AllSubscribed));
	let implicit_subscribed = implicit.clone();
	let implicit_explicit = implicit.clone();

	let all_subscribed = has_all_subscribed
		.then(|| {
			window
				.keys()
				.filter(|room_id| conn.subscriptions.contains_key(*room_id))
				.filter(move |room_id| {
					window
						.get(*room_id)
						.is_some_and(|room| !implicit_match(room, implicit_subscribed.as_ref()))
				})
		})
		.into_iter()
		.flatten()
		.map(AsRef::as_ref);

	let rooms_explicit = has_all_subscribed
		.is_false()
		.then(move || {
			explicit
				.into_iter()
				.flatten()
				.filter_map(|erc| extract_variant!(erc, ExtensionRoomConfig::Room))
				.filter(move |room_id| {
					window
						.get::<RoomId>(room_id.as_ref())
						.is_some_and(|room| !implicit_match(room, implicit_explicit.as_ref()))
				})
				.map(AsRef::as_ref)
		})
		.into_iter()
		.flatten();

	let rooms_selected = window
		.iter()
		.filter(move |(_, room)| implicit_match(room, implicit.as_ref()))
		.map(at!(0))
		.map(AsRef::as_ref);

	all_subscribed
		.chain(rooms_explicit)
		.chain(rooms_selected)
		.filter(|room_id| {
			window
				.get(*room_id)
				.is_some_and(|room| !room.forgotten)
		})
}

fn implicit_match<'a, ListIter>(room: &WindowRoom, implicit: Option<&ListIter>) -> bool
where
	ListIter: Iterator<Item = &'a ListId> + Clone,
{
	implicit.is_none_or(|lists| {
		lists
			.clone()
			.any(|list| room.lists.contains(list))
	})
}
