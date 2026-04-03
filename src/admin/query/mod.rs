mod account_data;
mod appservice;
mod globals;
mod oauth;
mod presence;
mod pusher;
mod raw;
mod resolver;
mod room_alias;
mod room_state_cache;
mod room_timeline;
mod sending;
mod short;
mod storage;
mod sync;
mod users;

use clap::Subcommand;
use tuwunel_core::Result;

use self::{
	account_data::AccountDataCommand, appservice::AppserviceCommand, globals::GlobalsCommand,
	oauth::OauthCommand, presence::PresenceCommand, pusher::PusherCommand, raw::RawCommand,
	resolver::ResolverCommand, room_alias::RoomAliasCommand,
	room_state_cache::RoomStateCacheCommand, room_timeline::RoomTimelineCommand,
	sending::SendingCommand, short::ShortCommand, storage::StorageCommand, sync::SyncCommand,
	users::UsersCommand,
};
use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
/// Query tables from database
pub(super) enum QueryCommand {
	/// - account_data.rs iterators and getters
	#[command(subcommand)]
	AccountData(AccountDataCommand),

	/// - appservice.rs iterators and getters
	#[command(subcommand)]
	Appservice(AppserviceCommand),

	/// - presence.rs iterators and getters
	#[command(subcommand)]
	Presence(PresenceCommand),

	/// - rooms/alias.rs iterators and getters
	#[command(subcommand)]
	RoomAlias(RoomAliasCommand),

	/// - rooms/state_cache iterators and getters
	#[command(subcommand)]
	RoomStateCache(RoomStateCacheCommand),

	/// - rooms/timeline iterators and getters
	#[command(subcommand)]
	RoomTimeline(RoomTimelineCommand),

	/// - globals.rs iterators and getters
	#[command(subcommand)]
	Globals(GlobalsCommand),

	/// - sending.rs iterators and getters
	#[command(subcommand)]
	Sending(SendingCommand),

	/// - users.rs iterators and getters
	#[command(subcommand)]
	Users(UsersCommand),

	/// - resolver service
	#[command(subcommand)]
	Resolver(ResolverCommand),

	/// - pusher service
	#[command(subcommand)]
	Pusher(PusherCommand),

	/// - short service
	#[command(subcommand)]
	Short(ShortCommand),

	/// - storage service
	#[command(subcommand)]
	Storage(StorageCommand),

	/// - sync service
	#[command(subcommand)]
	Sync(SyncCommand),

	/// - oauth service
	#[command(subcommand)]
	Oauth(OauthCommand),

	/// - raw service
	#[command(subcommand)]
	Raw(RawCommand),
}
