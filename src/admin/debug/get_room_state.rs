use futures::TryStreamExt;
use ruma::{OwnedRoomOrAliasId, events::AnyStateEvent, serde::Raw};
use tuwunel_core::{Error, Result, utils::stream::TryReadyExt};
use tuwunel_matrix::Event;

use crate::admin_command;

#[admin_command]
pub(super) async fn get_room_state(
	&self,
	room: OwnedRoomOrAliasId,
	kind: Option<String>,
	state_key: Option<String>,
) -> Result {
	let room_id = self.services.alias.maybe_resolve(&room).await?;

	if state_key.is_none()
		&& let Some(kind) = kind.clone().map(Into::into)
	{
		return self
			.services
			.state_accessor
			.room_state_type_pdus(&room_id, &kind)
			.map_ok(Event::into_format)
			.ready_and_then(|event: Raw<AnyStateEvent>| {
				serde_json::to_value(&event).map_err(Error::from)
			})
			.ready_and_then(|event| serde_json::to_string_pretty(&event).map_err(Error::from))
			.try_for_each(|json| writeln!(self, "```json\n{json}\n```"))
			.await;
	}

	if let Some(state_key) = state_key
		&& let Some(kind) = kind.map(Into::into)
	{
		let event: Raw<AnyStateEvent> = self
			.services
			.state_accessor
			.room_state_get(&room_id, &kind, &state_key)
			.await?
			.into_format();

		let value = serde_json::to_value(&event)?;
		let json = serde_json::to_string_pretty(&value)?;
		return writeln!(self, "```json\n{json}\n```").await;
	}

	self.services
		.state_accessor
		.room_state_full_pdus(&room_id)
		.map_ok(Event::into_format)
		.ready_and_then(|event: Raw<AnyStateEvent>| {
			serde_json::to_value(&event).map_err(Error::from)
		})
		.ready_and_then(|event| serde_json::to_string_pretty(&event).map_err(Error::from))
		.try_for_each(|json| writeln!(self, "```json\n{json}\n```"))
		.await
}
