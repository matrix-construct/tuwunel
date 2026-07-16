use axum::extract::State;
use ruma::UInt;
use synapse_admin_api::media::delete_media_by_date_size::v1 as delete_media_by_date_size;
use tuwunel_core::{Err, Result};

use super::MIN_BEFORE_TS;
use crate::{Ruma, RumaResponse, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/media/delete`
pub(crate) async fn admin_delete_media_by_date_size_route(
	State(services): State<crate::State>,
	body: Ruma<delete_media_by_date_size::Request>,
) -> Result<RumaResponse<delete_media_by_date_size::Response>> {
	require_admin(&services, body.sender_user()).await?;

	if u64::from(body.before_ts) < MIN_BEFORE_TS {
		return Err!(Request(InvalidParam(
			"before_ts is before the year 1970, this is likely a mistake"
		)));
	}

	let size_gt = body.size_gt.map(u64::from).unwrap_or(0);
	let keep_profiles = body.keep_profiles.unwrap_or(true);

	let deleted = services
		.media
		.delete_by_date_size(u64::from(body.before_ts), size_gt, keep_profiles)
		.await?;

	let total = UInt::try_from(deleted.len()).unwrap_or(UInt::MAX);

	let deleted_media = deleted
		.iter()
		.filter_map(|mxc| mxc.media_id().ok())
		.map(ToOwned::to_owned)
		.collect();

	Ok(RumaResponse(delete_media_by_date_size::Response { deleted_media, total }))
}
