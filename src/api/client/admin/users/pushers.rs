use axum::extract::State;
use futures::StreamExt;
use ruma::{
	OwnedDeviceId,
	api::client::push::{Pusher as RumaPusher, PusherKind},
	serde::JsonObject,
};
use synapse_admin_api::users::pushers::v1 as pushers;
use tuwunel_core::{
	Result,
	utils::{IterStream, math::ruma_from_usize, stream::WidebandExt},
};

use crate::{Ruma, client::admin::require_admin};

/// # `GET /_synapse/admin/v1/users/{user_id}/pushers`
///
/// MSC3881 is unimplemented, so `enabled` is always true; `device_id` is
/// resolved per pusher from its pushkey.
pub(crate) async fn admin_pushers_route(
	State(services): State<crate::State>,
	body: Ruma<pushers::Request>,
) -> Result<pushers::Response> {
	require_admin(&services, body.sender_user()).await?;

	let list: Vec<pushers::Pusher> = services
		.pusher
		.get_pushers(&body.user_id)
		.await
		.into_iter()
		.stream()
		.wide_then(async |pusher| {
			let device_id = services
				.pusher
				.get_pusher_device(&pusher.ids.pushkey)
				.await
				.ok();

			admin_pusher(pusher, device_id)
		})
		.collect()
		.await;

	let total = ruma_from_usize(list.len());

	Ok(pushers::Response::new(list, total))
}

/// Projects a Matrix pusher into the Synapse admin pusher shape.
fn admin_pusher(pusher: RumaPusher, device_id: Option<OwnedDeviceId>) -> pushers::Pusher {
	let (kind, data) = split_kind(&pusher.kind);

	pushers::Pusher {
		app_display_name: pusher.app_display_name.to_string(),
		app_id: pusher.ids.app_id,
		data,
		device_display_name: pusher.device_display_name.to_string(),
		kind,
		lang: Some(pusher.lang.to_string()),
		profile_tag: pusher
			.profile_tag
			.map(|tag| tag.to_string())
			.unwrap_or_default(),
		pushkey: pusher.ids.pushkey,
		enabled: true,
		device_id,
	}
}

/// Recovers the kind string and data object from the pusher kind's wire form.
fn split_kind(kind: &PusherKind) -> (String, Option<JsonObject>) {
	let Ok(serde_json::Value::Object(mut map)) = serde_json::to_value(kind) else {
		return (String::new(), None);
	};

	let kind = match map.remove("kind") {
		| Some(serde_json::Value::String(kind)) => kind,
		| _ => String::new(),
	};

	let data = match map.remove("data") {
		| Some(serde_json::Value::Object(data)) => Some(data),
		| _ => None,
	};

	(kind, data)
}

#[cfg(test)]
mod tests {
	use ruma::{
		api::client::push::{Pusher, PusherIds, PusherKind},
		push::HttpPusherData,
	};

	use super::{admin_pusher, split_kind};

	fn http_pusher() -> Pusher {
		Pusher {
			ids: PusherIds::new("pushkey123".to_owned(), "im.vector.app".to_owned()),
			kind: PusherKind::Http(HttpPusherData::new("https://push.example".to_owned())),
			app_display_name: "Element".into(),
			device_display_name: "Phone".into(),
			profile_tag: None,
			lang: "en".into(),
		}
	}

	#[test]
	fn split_kind_recovers_kind_and_data() {
		let (kind, data) =
			split_kind(&PusherKind::Http(HttpPusherData::new("https://push.example".to_owned())));

		assert_eq!(kind, "http");
		let data = data.expect("http pusher carries a data object");
		assert_eq!(data.get("url").and_then(|v| v.as_str()), Some("https://push.example"));
	}

	#[test]
	fn admin_pusher_maps_required_fields() {
		let mapped = admin_pusher(http_pusher(), None);

		assert_eq!(mapped.app_id, "im.vector.app");
		assert_eq!(mapped.pushkey, "pushkey123");
		assert_eq!(mapped.app_display_name, "Element");
		assert_eq!(mapped.device_display_name, "Phone");
		assert_eq!(mapped.kind, "http");
		assert_eq!(mapped.lang.as_deref(), Some("en"));
		assert_eq!(mapped.profile_tag, "");
		assert!(mapped.enabled);
		assert!(mapped.device_id.is_none());
		assert!(mapped.data.is_some());
	}
}
