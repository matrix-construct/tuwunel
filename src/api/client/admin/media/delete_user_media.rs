use axum::extract::State;
use futures::StreamExt;
use ruma::UInt;
use synapse_admin_api::media::delete_user_media::v1 as delete_user_media;
use tuwunel_core::{
	Err, Result,
	utils::stream::{IterStream, WidebandExt},
};

use super::{select_page, usize_from};
use crate::{Ruma, client::admin::require_admin};

/// # `DELETE /_synapse/admin/v1/users/{user_id}/media`
///
/// Deletes a single page of the user's media per call; callers loop to drain,
/// mirroring Synapse (the remaining media reorders after each page, so there is
/// no continuation token).
pub(crate) async fn admin_delete_user_media_route(
	State(services): State<crate::State>,
	body: Ruma<delete_user_media::Request>,
) -> Result<delete_user_media::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services
		.globals
		.server_is_ours(body.user_id.server_name())
	{
		return Err!(Request(InvalidParam("Can only look up local users")));
	}

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("User not found")));
	}

	let entries = services.media.user_media(&body.user_id).await?;

	let from = body.from.map(usize_from).unwrap_or(0);
	let limit = body.limit.map(usize_from).unwrap_or(100);

	let page = select_page(entries, body.order_by.as_ref(), body.dir, from, limit);

	let deleted_media: Vec<String> = page
		.iter()
		.stream()
		.wide_filter_map(async |entry| {
			let mxc = entry.mxc.parts().ok()?;

			services
				.media
				.delete(&mxc)
				.await
				.ok()
				.map(|()| mxc.media_id.to_owned())
		})
		.collect()
		.await;

	let total = UInt::try_from(deleted_media.len()).unwrap_or(UInt::MAX);

	Ok(delete_user_media::Response { deleted_media, total })
}
