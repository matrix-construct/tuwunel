mod account_data;
mod appservice;
mod feds;
mod globals;
mod oauth;
mod peer_status;
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
mod threepid;
mod users;

use clap::Subcommand;
use tuwunel_core::Result;

use self::{
	account_data::AccountDataCommand, appservice::AppserviceCommand, feds::FedsCommand,
	globals::GlobalsCommand, oauth::OauthCommand, peer_status::PeerStatusCommand,
	presence::PresenceCommand, pusher::PusherCommand, raw::RawCommand, resolver::ResolverCommand,
	room_alias::RoomAliasCommand, room_state_cache::RoomStateCacheCommand,
	room_timeline::RoomTimelineCommand, sending::SendingCommand, short::ShortCommand,
	storage::StorageCommand, sync::SyncCommand, threepid::ThreepidCommand, users::UsersCommand,
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

	/// - federation fanout diagnostics
	#[command(subcommand)]
	Feds(FedsCommand),

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

	/// - threepid service
	#[command(subcommand)]
	Threepid(ThreepidCommand),

	/// - resolver service
	#[command(subcommand)]
	Resolver(ResolverCommand),

	/// - per-server reachability store on the federation service
	#[command(subcommand)]
	PeerStatus(PeerStatusCommand),

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
