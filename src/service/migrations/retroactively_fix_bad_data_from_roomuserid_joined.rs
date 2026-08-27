use futures::StreamExt;
use ruma::events::room::member::MembershipState;
use tuwunel_core::{
	Result, debug_info, info,
	matrix::PduCount,
	utils::{ReadyExt, stream::BroadbandExt},
	warn,
};

use crate::Services;

pub(super) async fn retroactively_fix_bad_data_from_roomuserid_joined(
	services: &Services,
) -> Result {
	warn!("Retroactively fixing bad data from broken roomuserid_joined");

	let db = &services.db;
	let _cork = db.cork_and_sync();

	// The room count is cheap beside the per-member work, so the total is exact.
	let total = services.metadata.iter_ids().count().await;

	services
		.server
		.progress
		.expect_total(u64::try_from(total).unwrap_or(u64::MAX));

	services
		.metadata
		.iter_ids()
		.for_each(async |room_id| {
			debug_info!(%room_id, "Fixing room");

			services
				.state_cache
				.room_members(room_id)
				.map(ToOwned::to_owned)
				.broad_filter_map(async |user_id| {
					// A member with no resolved member event is left untouched.
					let member = services
						.state_accessor
						.get_member(room_id, &user_id)
						.await
						.ok()?;

					Some((user_id, member.membership))
				})
				.ready_for_each(|(user_id, membership)| {
					let count = services.globals.next_count();

					match membership {
						| MembershipState::Join => services.state_cache.mark_as_joined(
							&user_id,
							room_id,
							PduCount::Normal(*count),
						),
						| _ => services.state_cache.mark_as_left(
							&user_id,
							room_id,
							PduCount::Normal(*count),
						),
					}
				})
				.await;

			services
				.state_cache
				.update_joined_count(room_id)
				.await;

			services.server.progress.advance();
		})
		.await;

	info!("Finished fixing");

	db["global"].insert(b"retroactively_fix_bad_data_from_roomuserid_joined", []);
	Ok(())
}
