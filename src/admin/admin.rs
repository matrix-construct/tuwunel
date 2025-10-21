use clap::Parser;
use tuwunel_core::Result;
use tuwunel_macros::command_dispatch;

use crate::{
	appservices, appservices::AppserviceCommand, check, check::CheckCommand, debug,
	debug::DebugCommand, federation, federation::FederationCommand, media, media::MediaCommand,
	query, query::QueryCommand, rooms, rooms::RoomCommand, server, server::ServerCommand, users,
	users::UserCommand,
};

#[derive(Debug, Parser)]
#[command(name = "admin", version = tuwunel_core::version())]
#[command(arg_required_else_help = true)]
#[command_dispatch]
pub(super) enum AdminCommand {
	#[command(subcommand)]
	/// - Commands for managing appservices
	Appservices(AppserviceCommand),

	#[command(subcommand)]
	/// - Commands for managing local users
	Users(UserCommand),

	#[command(subcommand)]
	/// - Commands for managing rooms
	Rooms(RoomCommand),

	#[command(subcommand)]
	/// - Commands for managing federation
	Federation(FederationCommand),

	#[command(subcommand)]
	/// - Commands for managing the server
	Server(ServerCommand),

	#[command(subcommand)]
	/// - Commands for managing media
	Media(MediaCommand),

	#[command(subcommand)]
	/// - Commands for checking integrity
	Check(CheckCommand),

	#[command(subcommand)]
	/// - Commands for debugging things
	Debug(DebugCommand),

	#[command(subcommand)]
	/// - Low-level queries for database getters and iterators
	Query(QueryCommand),
}
