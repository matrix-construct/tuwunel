use futures::{FutureExt, StreamExt, TryFutureExt, join};
use ruma::{
	OwnedRoomAliasId, OwnedRoomOrAliasId, RoomAliasId,
	events::{
		StateEventType,
		room::{canonical_alias::RoomCanonicalAliasEventContent, power_levels::UserPowerLevel},
	},
};
use tuwunel_core::{Err, Result, utils::TryFutureExtExt};

use crate::admin_command;

#[admin_command]
pub(super) async fn room_info(&self, room: OwnedRoomOrAliasId) -> Result {
	let room_id = self.services.alias.maybe_resolve(&room).await?;

	if !self.services.metadata.exists(&room_id).await {
		return Err!("Room {room_id} is not known to this server.");
	}

	let create_event = self
		.services
		.state_accessor
		.get_create(&room_id)
		.boxed();

	let name = self
		.services
		.state_accessor
		.get_name(&room_id)
		.unwrap_or_else(|_| "(none)".to_owned())
		.boxed();

	let topic = self
		.services
		.state_accessor
		.get_room_topic(&room_id)
		.unwrap_or_else(|_| "(none)".to_owned())
		.boxed();

	let canonical_alias = self
		.services
		.state_accessor
		.room_state_get_content(&room_id, &StateEventType::RoomCanonicalAlias, "")
		.map_ok(|content: RoomCanonicalAliasEventContent| (content.alias, content.alt_aliases))
		.unwrap_or_default()
		.boxed();

	let aliases = self
		.services
		.alias
		.local_aliases_for_room(&room_id)
		.map(Into::into)
		.collect::<Vec<OwnedRoomAliasId>>()
		.boxed();

	let power_levels = self
		.services
		.state_accessor
		.get_power_levels(&room_id)
		.boxed();

	let (create_event, name, topic, (canonical_alias, alt_aliases), mut aliases, power_levels) =
		join!(create_event, name, topic, canonical_alias, aliases, power_levels);

	let create_event = create_event?;
	let power_levels = power_levels?;

	let room_version = create_event.room_version()?;

	aliases.sort();

	// Local aliases not published in the canonical alias event.
	let unlisted: Vec<_> = aliases
		.iter()
		.filter(|&alias| canonical_alias.as_ref() != Some(alias) && !alt_aliases.contains(alias))
		.collect();

	let state_default = power_levels.state_default;

	let mut admins: Vec<_> = power_levels
		.users
		.keys()
		.chain(
			power_levels
				.rules
				.privileged_creators
				.iter()
				.flatten(),
		)
		.map(|user_id| (user_id, power_levels.for_user(user_id)))
		.filter(|&(_, pl)| pl >= state_default)
		.collect();

	admins.sort_by(|(user_a, pl_a), (user_b, pl_b)| {
		pl_b.cmp(pl_a).then_with(|| user_a.cmp(user_b))
	});

	writeln!(self, "```\nRoom information for {room_id}\n").await?;

	writeln!(self, "Room version: {room_version}\n").await?;

	writeln!(self, "Name: {name}").await?;
	writeln!(self, "Topic: \n{topic}\n").await?;

	writeln!(self, "Aliases:").await?;
	let canonical_alias = canonical_alias
		.as_deref()
		.map(RoomAliasId::as_str)
		.unwrap_or("none");
	writeln!(self, "  Canonical: {canonical_alias}").await?;

	writeln!(self, "  Alternative:").await?;
	if alt_aliases.is_empty() {
		writeln!(self, "    none").await?;
	} else {
		for alias in &alt_aliases {
			writeln!(self, "    - {alias}").await?;
		}
	}

	writeln!(self, "  Local unlisted:").await?;
	if unlisted.is_empty() {
		writeln!(self, "    none").await?;
	} else {
		for alias in unlisted {
			writeln!(self, "    - {alias}").await?;
		}
	}

	writeln!(self, "\nAdmins (power level >= {state_default}):").await?;
	for (user_id, pl) in &admins {
		let pl = match pl {
			| UserPowerLevel::Int(pl) => pl.to_string(),
			| UserPowerLevel::Infinite => "creator".to_owned(),
		};

		writeln!(self, "  - {user_id} ({pl})").await?;
	}

	writeln!(self, "```").await?;

	Ok(())
}
