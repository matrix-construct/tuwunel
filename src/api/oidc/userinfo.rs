use axum::{Json, extract::State, response::IntoResponse};
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Bearer},
};
use futures::future::join;
use serde_json::json;
use tuwunel_core::{Err, Result, utils::future::TryExtExt};

pub(crate) async fn userinfo_route(
	State(services): State<crate::State>,
	TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Result<impl IntoResponse> {
	let token = bearer.token();
	let Ok((user_id, _device_id, _expires)) = services.users.find_from_token(token).await else {
		return Err!(Request(Unauthorized("Invalid access token")));
	};

	let avatar_url = services.users.avatar_url(&user_id).ok();

	let displayname = services.users.displayname(&user_id).ok();

	let (avatar_url, displayname) = join(avatar_url, displayname).await;

	Ok(Json(json!({
		"sub": user_id.to_string(),
		"name": displayname,
		"picture": avatar_url,
	})))
}
