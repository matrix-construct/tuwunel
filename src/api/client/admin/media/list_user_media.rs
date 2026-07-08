use axum::extract::State;
use ruma::UInt;
use synapse_admin_api::media::list_user_media::v1::{self as list_user_media, UserMedia};
use tuwunel_core::{Err, Result};
use tuwunel_service::media::UserMediaEntry;

use super::{select_page, usize_from};
use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/users/{user_id}/media`
pub(crate) async fn admin_list_user_media_route(
	State(services): State<crate::State>,
	body: Ruma<list_user_media::Request>,
) -> Result<list_user_media::Response> {
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

	let total = UInt::try_from(entries.len()).unwrap_or(UInt::MAX);

	let from = body.from.map(usize_from).unwrap_or(0);
	let limit = body.limit.map(usize_from).unwrap_or(100);

	let page = select_page(entries, body.order_by.as_ref(), body.dir, from, limit);

	let next_token = (from.saturating_add(page.len()) < usize_from(total))
		.then(|| UInt::try_from(from.saturating_add(page.len())).unwrap_or(UInt::MAX));

	let media = page.into_iter().map(into_user_media).collect();

	Ok(list_user_media::Response { media, next_token, total })
}

fn into_user_media(entry: UserMediaEntry) -> UserMedia {
	UserMedia {
		media_id: entry
			.mxc
			.media_id()
			.unwrap_or_default()
			.to_owned(),
		media_type: entry.media_type.unwrap_or_default(),
		media_length: entry
			.media_length
			.and_then(|len| UInt::try_from(len).ok()),
		upload_name: entry.upload_name.unwrap_or_default(),
		created_ts: UInt::try_from(entry.created_ts).unwrap_or(UInt::MAX),
		url_cache: None,
		last_access_ts: UInt::from(0_u32),
		quarantined_by: None,
		safe_from_quarantine: false,
		user_id: Some(entry.user_id),
		authenticated: None,
		sha256: None,
	}
}
