use axum::extract::State;
use ruma::{Mxc, UInt};
use synapse_admin_api::media::delete_media::v1 as delete_media;
use tuwunel_core::{Err, Result, err};

use crate::{Ruma, client::admin::require_admin};

/// # `DELETE /_synapse/admin/v1/media/{server_name}/{media_id}`
pub(crate) async fn admin_delete_media_route(
	State(services): State<crate::State>,
	body: Ruma<delete_media::Request>,
) -> Result<delete_media::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.globals.server_is_ours(&body.server_name) {
		return Err!(Request(InvalidParam("Can only delete local media")));
	}

	let mxc = Mxc {
		server_name: &body.server_name,
		media_id: &body.media_id,
	};

	services
		.media
		.delete(&mxc)
		.await
		.map_err(|_| err!(Request(NotFound("Media not found"))))?;

	Ok(delete_media::Response {
		deleted_media: vec![body.media_id.clone()],
		total: UInt::from(1_u32),
	})
}
