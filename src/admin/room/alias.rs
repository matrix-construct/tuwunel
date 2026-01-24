use std::fmt::Write;

use clap::Subcommand;
use futures::StreamExt;
use ruma::{OwnedRoomAliasId, OwnedRoomId};
use tuwunel_core::{Err, Result};

use crate::Context;

#[derive(Debug, Subcommand)]
pub(crate) enum RoomAliasCommand {
	/// - Make an alias point to a room.
	Set {
		#[arg(short, long)]
		/// Set the alias even if a room is already using it
		force: bool,

		/// The room id to set the alias on
		room_id: OwnedRoomId,

		/// The alias localpart to use (`alias`, not `#alias:servername.tld`)
		room_alias_localpart: String,
	},

	/// - Remove a local alias
	Remove {
		/// The alias localpart to remove (`alias`, not `#alias:servername.tld`)
		room_alias_localpart: String,
	},

	/// - Show which room is using an alias
	Which {
		/// The alias localpart to look up (`alias`, not
		/// `#alias:servername.tld`)
		room_alias_localpart: String,
	},

	/// - List aliases currently being used
	List {
		/// If set, only list the aliases for this room
		room_id: Option<OwnedRoomId>,
	},
}

pub(super) async fn process(command: RoomAliasCommand, context: &Context<'_>) -> Result {
	let services = context.services;

	match command {
		| RoomAliasCommand::Set { ref room_alias_localpart, .. }
		| RoomAliasCommand::Remove { ref room_alias_localpart }
		| RoomAliasCommand::Which { ref room_alias_localpart } => {
			let room_alias_str =
				format!("#{}:{}", room_alias_localpart, services.globals.server_name());
			let room_alias = match OwnedRoomAliasId::parse(room_alias_str) {
				| Ok(alias) => alias,
				| Err(err) => {
					return Err!("Failed to parse alias: {err}");
				},
			};
			match command {
				| RoomAliasCommand::Set { force, room_id, .. } => {
					match (
						force,
						services
							.alias
							.resolve_local_alias(&room_alias)
							.await,
					) {
						| (true, Ok(id)) => {
							match services.alias.set_alias(&room_alias, &room_id) {
								| Err(err) => Err!("Failed to remove alias: {err}"),
								| Ok(()) =>
									context
										.write_str(&format!(
											"Successfully overwrote alias (formerly {id})"
										))
										.await,
							}
						},
						| (false, Ok(id)) => Err!(
							"Refusing to overwrite in use alias for {id}, use -f or --force to \
							 overwrite"
						),
						| (_, Err(_)) => match services.alias.set_alias(&room_alias, &room_id) {
							| Err(err) => Err!("Failed to remove alias: {err}"),
							| Ok(()) => context.write_str("Successfully set alias").await,
						},
					}
				},
				| RoomAliasCommand::Remove { .. } => {
					match services
						.alias
						.resolve_local_alias(&room_alias)
						.await
					{
						| Err(_) => Err!("Alias isn't in use."),
						| Ok(id) => match services.alias.remove_alias(&room_alias).await {
							| Err(err) => Err!("Failed to remove alias: {err}"),
							| Ok(()) =>
								context
									.write_str(&format!("Removed alias from {id}"))
									.await,
						},
					}
				},
				| RoomAliasCommand::Which { .. } => {
					match services
						.alias
						.resolve_local_alias(&room_alias)
						.await
					{
						| Err(_) => Err!("Alias isn't in use."),
						| Ok(id) =>
							context
								.write_str(&format!("Alias resolves to {id}"))
								.await,
					}
				},
				| RoomAliasCommand::List { .. } => unreachable!(),
			}
		},
		| RoomAliasCommand::List { room_id } =>
			if let Some(room_id) = room_id {
				let aliases: Vec<OwnedRoomAliasId> = services
					.alias
					.local_aliases_for_room(&room_id)
					.map(Into::into)
					.collect()
					.await;

				let plain_list = aliases
					.iter()
					.fold(String::new(), |mut output, alias| {
						writeln!(output, "- {alias}")
							.expect("should be able to write to string buffer");
						output
					});

				let plain = format!("Aliases for {room_id}:\n{plain_list}");
				context.write_str(&plain).await
			} else {
				let aliases = services
					.alias
					.all_local_aliases()
					.map(|(room_id, localpart)| (room_id.into(), localpart.into()))
					.collect::<Vec<(OwnedRoomId, String)>>()
					.await;

				let server_name = services.globals.server_name();
				let plain_list = aliases
					.iter()
					.fold(String::new(), |mut output, (alias, id)| {
						writeln!(output, "- `{alias}` -> #{id}:{server_name}")
							.expect("should be able to write to string buffer");
						output
					});

				let plain = format!("Aliases:\n{plain_list}");
				context.write_str(&plain).await
			},
	}
}
