use std::{collections::HashSet, sync::Arc};

use futures::{FutureExt, StreamExt, future::join};
use ruma::{
	RoomId, UserId,
	api::client::push::ProfileTag,
	events::{GlobalAccountDataEventType, TimelineEventType, push_rules::PushRulesEvent},
	push::{Action, Actions, Ruleset, Tweak},
};
use serde::{Deserialize, Serialize};
use tuwunel_core::{
	Result, implement,
	matrix::{
		event::Event,
		pdu::{Count, Pdu, PduId, RawPduId},
	},
	utils::{BoolExt, ReadyExt, future::TryExtExt, time::now_millis},
};
use tuwunel_database::{Deserialized, Json, Map};

use crate::rooms::short::ShortRoomId;

/// Succinct version of Ruma's Notification. Appended to the database when the
/// user is notified. The PduCount is part of the database key so only the
/// shortroomid is included. Together they  make the PduId.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Notified {
	/// Milliseconds time at which the event notification was sent.
	pub ts: u64,

	/// ShortRoomId
	pub sroomid: ShortRoomId,

	/// The profile tag of the rule that matched this event.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tag: Option<ProfileTag>,

	/// Actions vector
	pub actions: Actions,
}

/// Called by timeline append_pdu.
#[implement(super::Service)]
#[tracing::instrument(name = "append", level = "debug", skip_all)]
pub(crate) async fn append_pdu(&self, pdu_id: RawPduId, pdu: &Pdu) -> Result {
	// Don't notify the sender of their own events, and dont send from ignored users
	let push_target = self
		.services
		.state_cache
		.active_local_users_in_room(pdu.room_id())
		.map(ToOwned::to_owned)
		.ready_filter(|user| *user != pdu.sender())
		.filter_map(async |recipient_user| {
			self.services
				.users
				.user_is_ignored(pdu.sender(), &recipient_user)
				.await
				.is_false()
				.then_some(recipient_user)
		})
		.collect::<HashSet<_>>();

	let power_levels = self
		.services
		.state_accessor
		.get_power_levels(pdu.room_id())
		.ok();

	let (mut push_target, power_levels) = join(push_target, power_levels).boxed().await;

	if *pdu.kind() == TimelineEventType::RoomMember
		&& let Some(Ok(target_user_id)) = pdu.state_key().map(UserId::parse)
		&& self
			.services
			.users
			.is_active_local(target_user_id)
			.await
	{
		push_target.insert(target_user_id.to_owned());
	}

	let serialized = pdu.to_format();
	let _cork = self.db.db.cork();
	for user in &push_target {
		let rules_for_user = self
			.services
			.account_data
			.get_global(user, GlobalAccountDataEventType::PushRules)
			.await
			.map_or_else(
				|_| Ruleset::server_default(user),
				|ev: PushRulesEvent| ev.content.global,
			);

		let actions = self
			.services
			.pusher
			.get_actions(user, &rules_for_user, power_levels.as_ref(), &serialized, pdu.room_id())
			.await;

		let notify = actions
			.iter()
			.any(|action| matches!(action, Action::Notify));

		let highlight = actions
			.iter()
			.any(|action| matches!(action, Action::SetTweak(Tweak::Highlight(true))));

		let increment_notify =
			notify.then_async(|| self.increment_notificationcount(pdu.room_id(), user));

		let increment_highlight =
			highlight.then_async(|| self.increment_highlightcount(pdu.room_id(), user));

		join(increment_notify, increment_highlight).await;

		if notify || highlight {
			let id: PduId = pdu_id.into();
			let notified = Notified {
				ts: now_millis(),
				sroomid: id.shortroomid,
				tag: None,
				actions: actions.into(),
			};

			if matches!(id.count, Count::Normal(_)) {
				self.db
					.useridcount_notification
					.put((user, id.count.into_unsigned()), Json(notified));
			}
		}

		if notify || highlight || self.services.config.push_everything {
			self.services
				.pusher
				.get_pushkeys(user)
				.map(ToOwned::to_owned)
				.ready_for_each(|push_key| {
					self.services
						.sending
						.send_pdu_push(&pdu_id, user, push_key)
						.expect("TODO: replace with future");
				})
				.await;
		}
	}

	Ok(())
}

#[implement(super::Service)]
async fn increment_notificationcount(&self, room_id: &RoomId, user_id: &UserId) {
	let db = &self.db.userroomid_notificationcount;
	let key = (room_id.to_owned(), user_id.to_owned());
	let _lock = self.notification_increment_mutex.lock(&key).await;

	increment(db, (user_id, room_id)).await;
}

#[implement(super::Service)]
async fn increment_highlightcount(&self, room_id: &RoomId, user_id: &UserId) {
	let db = &self.db.userroomid_highlightcount;
	let key = (room_id.to_owned(), user_id.to_owned());
	let _lock = self.highlight_increment_mutex.lock(&key).await;

	increment(db, (user_id, room_id)).await;
}

async fn increment(db: &Arc<Map>, key: (&UserId, &RoomId)) {
	let old: u64 = db.qry(&key).await.deserialized().unwrap_or(0);
	let new = old.saturating_add(1);
	db.put(key, new);
}
