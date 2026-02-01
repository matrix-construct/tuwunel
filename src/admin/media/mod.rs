#![expect(rustdoc::broken_intra_doc_links)]
mod commands;

use clap::Subcommand;
use ruma::{OwnedEventId, OwnedMxcUri, OwnedServerName};
use tuwunel_core::Result;

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(super) enum MediaCommand {
	/// - Deletes a single media file from our database and on the filesystem
	///   via a single MXC URL or event ID (not redacted)
	Delete {
		/// The MXC URL to delete
		#[arg(long)]
		mxc: Option<OwnedMxcUri>,

		/// - The message event ID which contains the media and thumbnail MXC
		///   URLs
		#[arg(long)]
		event_id: Option<OwnedEventId>,
	},

	/// - Deletes a codeblock list of MXC URLs from our database and on the
	///   filesystem. This will always ignore errors.
	DeleteList,

	/// - Deletes all remote (and optionally local) media created before or
	///   after [duration] time using filesystem metadata first created at date,
	///   or fallback to last modified date. This will always ignore errors by
	///   default.
	DeletePastRemoteMedia {
		/// - The relative time (e.g. 30s, 5m, 7d) within which to search
		duration: String,

		/// - Only delete media created before [duration] ago
		#[arg(long, short)]
		before: bool,

		/// - Only delete media created after [duration] ago
		#[arg(long, short)]
		after: bool,

		/// - Long argument to additionally delete local media
		#[arg(long)]
		yes_i_want_to_delete_local_media: bool,
	},

	/// - Deletes all the local media from a local user on our server. This will
	///   always ignore errors by default.
	DeleteAllFromUser {
		username: String,
	},

	/// - Deletes all remote media from the specified remote server. This will
	///   always ignore errors by default.
	DeleteAllFromServer {
		server_name: OwnedServerName,

		/// Long argument to delete local media
		#[arg(long)]
		yes_i_want_to_delete_local_media: bool,
	},

	GetFileInfo {
		/// The MXC URL to lookup info for.
		mxc: OwnedMxcUri,
	},

	GetRemoteFile {
		/// The MXC URL to fetch
		mxc: OwnedMxcUri,

		#[arg(short, long)]
		server: Option<OwnedServerName>,

		#[arg(short, long, default_value("10000"))]
		timeout: u32,
	},

	GetRemoteThumbnail {
		/// The MXC URL to fetch
		mxc: OwnedMxcUri,

		#[arg(short, long)]
		server: Option<OwnedServerName>,

		#[arg(short, long, default_value("10000"))]
		timeout: u32,

		#[arg(long, default_value("800"))]
		width: u32,

		#[arg(long, default_value("800"))]
		height: u32,
	},
}
