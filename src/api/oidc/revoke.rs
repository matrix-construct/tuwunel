use axum::{Json, extract::State, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;
use tuwunel_core::Result;

#[derive(Debug, Deserialize)]
pub(crate) struct RevokeRequest {
	token: String,
	#[serde(default, rename = "token_type_hint")]
	_token_type_hint: Option<String>,
}

pub(crate) async fn revoke_route(
	State(services): State<crate::State>,
	axum::extract::Form(body): axum::extract::Form<RevokeRequest>,
) -> Result<impl IntoResponse> {
	if let Ok((user_id, device_id, _)) = services.users.find_from_token(&body.token).await {
		services
			.users
			.remove_device(&user_id, &device_id)
			.await;
	}

	Ok(Json(json!({})))
}
