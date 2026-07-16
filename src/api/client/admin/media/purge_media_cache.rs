use std::time::Duration;

use axum::extract::State;
use ruma::UInt;
use synapse_admin_api::media::purge_media_cache::v1::{Request, Response};
use tuwunel_core::{Err, Result, utils::time::timepoint_from_epoch};

use super::MIN_BEFORE_TS;
use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/purge_media_cache`
///
/// Purges cached remote media older than `before_ts`. The cutoff compares file
/// modification time; last-access times are not tracked.
pub(crate) async fn admin_purge_media_cache_route(
	State(services): State<crate::State>,
	body: Ruma<Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let before_ts = u64::from(body.before_ts);
	if before_ts < MIN_BEFORE_TS {
		return Err!(Request(InvalidParam(
			"before_ts is before the year 1970, this is likely a mistake"
		)));
	}

	let cutoff = timepoint_from_epoch(Duration::from_millis(before_ts))?;

	let deleted = services
		.media
		.delete_range(cutoff, true, false, false)
		.await?;

	let deleted = UInt::try_from(deleted).unwrap_or(UInt::MAX);

	Ok(Response { deleted })
}
