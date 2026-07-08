use axum::extract::State;
use ruma::{CanonicalJsonObject, CanonicalJsonValue, serde::Raw};
use synapse_admin_api::events::fetch_event::v1 as fetch_event;
use tuwunel_core::{Result, err};

use crate::{Ruma, client::admin::require_admin};

const SOFT_FAILED: &str = "io.element.synapse.soft_failed";

pub(crate) async fn admin_fetch_event_route(
	State(services): State<crate::State>,
	body: Ruma<fetch_event::Request>,
) -> Result<fetch_event::Response> {
	require_admin(&services, body.sender_user()).await?;

	let mut event = services
		.timeline
		.get_pdu_json(&body.event_id)
		.await
		.map_err(|_| err!(Request(NotFound("Event not found"))))?;

	if services
		.pdu_metadata
		.is_event_soft_failed(&body.event_id)
		.await
	{
		mark_soft_failed(&mut event);
	}

	let event = serde_json::value::to_raw_value(&event)
		.map_err(|e| err!("Failed to serialize event: {e}"))?;

	Ok(fetch_event::Response::new(Raw::from_json(event)))
}

/// Sets Synapse's `io.element.synapse.soft_failed` marker inside `unsigned`,
/// creating the `unsigned` object when the event lacks one.
fn mark_soft_failed(event: &mut CanonicalJsonObject) {
	let Some(CanonicalJsonValue::Object(unsigned)) = event.get_mut("unsigned") else {
		let unsigned = [(SOFT_FAILED.into(), CanonicalJsonValue::Bool(true))].into();
		event.insert("unsigned".into(), CanonicalJsonValue::Object(unsigned));
		return;
	};

	unsigned.insert(SOFT_FAILED.into(), CanonicalJsonValue::Bool(true));
}

#[cfg(test)]
mod tests {
	use ruma::{CanonicalJsonObject, CanonicalJsonValue};

	use super::{SOFT_FAILED, mark_soft_failed};

	fn soft_failed(event: &CanonicalJsonObject) -> Option<bool> {
		event
			.get("unsigned")?
			.as_object()?
			.get(SOFT_FAILED)?
			.as_bool()
	}

	#[test]
	fn marks_event_without_unsigned() {
		let mut event = CanonicalJsonObject::new();

		mark_soft_failed(&mut event);

		assert_eq!(soft_failed(&event), Some(true));
	}

	#[test]
	fn marks_event_with_existing_unsigned() {
		let membership = CanonicalJsonValue::String("leave".into());
		let unsigned = [("prev_sender".into(), membership)].into();
		let mut event = [("unsigned".into(), CanonicalJsonValue::Object(unsigned))].into();

		mark_soft_failed(&mut event);

		assert_eq!(soft_failed(&event), Some(true));
		assert!(
			event
				.get("unsigned")
				.and_then(CanonicalJsonValue::as_object)
				.is_some_and(|u| u.contains_key("prev_sender"))
		);
	}
}
