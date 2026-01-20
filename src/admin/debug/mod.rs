mod commands;
pub(crate) mod tester;

use clap::Subcommand;
use ruma::{OwnedEventId, OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName};
use tuwunel_core::Result;
use tuwunel_service::rooms::short::ShortRoomId;

use self::tester::TesterCommand;
use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(super) enum DebugCommand {
	/// - Echo input of admin command
	Echo {
		message: Vec<String>,
	},

	/// - Get the auth_chain of a PDU
	GetAuthChain {
		/// An event ID (the $ character followed by the base64 reference hash)
		event_id: OwnedEventId,
	},

	/// - Parse and print a PDU from a JSON
	///
	/// The PDU event is only checked for validity and is not added to the
	/// database.
	///
	/// This command needs a JSON blob provided in a Markdown code block below
	/// the command.
	ParsePdu,

	/// - Retrieve and print a PDU by EventID from the tuwunel database
	GetPdu {
		/// An event ID (a $ followed by the base64 reference hash)
		event_id: OwnedEventId,
	},

	/// - Retrieve and print a PDU by PduId from the tuwunel database
	GetShortPdu {
		/// Shortroomid integer
		shortroomid: ShortRoomId,

		/// PduCount integer
		count: i64,
	},

	/// - Attempts to retrieve a PDU from a remote server. Inserts it into our
	///   database/timeline if found and we do not have this PDU already
	///   (following normal event auth rules, handles it as an incoming PDU).
	GetRemotePdu {
		/// An event ID (a $ followed by the base64 reference hash)
		event_id: OwnedEventId,

		/// Argument for us to attempt to fetch the event from the
		/// specified remote server.
		server: OwnedServerName,
	},

	/// - Same as `get-remote-pdu` but accepts a codeblock newline delimited
	///   list of PDUs and a single server to fetch from
	GetRemotePduList {
		/// Argument for us to attempt to fetch all the events from the
		/// specified remote server.
		server: OwnedServerName,

		/// If set, ignores errors, else stops at the first error/failure.
		#[arg(short, long)]
		force: bool,
	},

	/// - Gets all the room state events for the specified room.
	GetRoomState {
		/// Room ID
		room_id: OwnedRoomOrAliasId,
	},

	/// - Get and display signing keys from local cache or remote server.
	GetSigningKeys {
		server_name: Option<OwnedServerName>,

		#[arg(long)]
		notary: Option<OwnedServerName>,

		#[arg(short, long)]
		query: bool,
	},

	/// - Get and display signing keys from local cache or remote server.
	GetVerifyKeys {
		server_name: Option<OwnedServerName>,
	},

	/// - Sends a federation request to the remote server's
	///   `/_matrix/federation/v1/version` endpoint and measures the latency it
	///   took for the server to respond
	Ping {
		server: OwnedServerName,
	},

	/// - Forces device lists for all local and remote users to be updated (as
	///   having new keys available)
	ForceDeviceListUpdates,

	/// - Change tracing log level/filter on the fly
	///
	/// This accepts the same format as the `log` config option.
	ChangeLogLevel {
		/// Log level/filter
		filter: Option<String>,

		/// Resets the log level/filter to the one in your config
		#[arg(short, long)]
		reset: bool,
	},

	/// - Sign JSON blob
	///
	/// This command needs a JSON blob provided in a Markdown code block below
	/// the command.
	SignJson,

	/// - Verify JSON signatures
	///
	/// This command needs a JSON blob provided in a Markdown code block below
	/// the command.
	VerifyJson,

	/// - Verify PDU
	///
	/// This re-verifies a PDU existing in the database found by ID.
	VerifyPdu {
		event_id: OwnedEventId,
	},

	/// - Prints the very first PDU in the specified room (typically
	///   m.room.create)
	FirstPduInRoom {
		/// The room ID
		room_id: OwnedRoomId,
	},

	/// - Prints the latest ("last") PDU in the specified room (typically a
	///   message)
	LatestPduInRoom {
		/// The room ID
		room_id: OwnedRoomId,
	},

	/// - Forcefully replaces the room state of our local copy of the specified
	///   room, with the copy (auth chain and room state events) the specified
	///   remote server says.
	///
	/// A common desire for room deletion is to simply "reset" our copy of the
	/// room. While this admin command is not a replacement for that, if you
	/// know you have split/broken room state and you know another server in the
	/// room that has the best/working room state, this command can let you use
	/// their room state. Such example is your server saying users are in a
	/// room, but other servers are saying they're not in the room in question.
	///
	/// This command will get the latest PDU in the room we know about, and
	/// request the room state at that point in time via
	/// `/_matrix/federation/v1/state/{roomId}`.
	ForceSetRoomStateFromServer {
		/// The impacted room ID
		room_id: OwnedRoomId,
		/// The server we will use to query the room state for
		server_name: OwnedServerName,
	},

	/// - Runs a server name through tuwunel's true destination resolution
	///   process
	///
	/// Useful for debugging well-known issues
	ResolveTrueDestination {
		server_name: OwnedServerName,

		#[arg(short, long)]
		no_cache: bool,
	},

	/// - Print extended memory usage
	///
	/// Optional argument is a character mask (a sequence of characters in any
	/// order) which enable additional extended statistics. Known characters are
	/// "abdeglmx". For convenience, a '*' will enable everything.
	MemoryStats {
		opts: Option<String>,
	},

	/// - Print general tokio runtime metric totals.
	RuntimeMetrics,

	/// - Print detailed tokio runtime metrics accumulated since last command
	///   invocation.
	RuntimeInterval,

	/// - Print the current time
	Time,

	/// - List dependencies
	ListDependencies {
		#[arg(short, long)]
		names: bool,
	},

	/// - Get database statistics
	DatabaseStats {
		property: Option<String>,

		#[arg(short, long, alias("column"))]
		map: Option<String>,
	},

	/// - Trim memory usage
	TrimMemory,

	/// - List database files
	DatabaseFiles {
		map: Option<String>,

		#[arg(long)]
		level: Option<i32>,
	},

	/// - Create a JWT token for login
	CreateJwt {
		/// Localpart of the user's MXID
		user: String,

		/// Set expiration time in seconds from now.
		#[arg(long)]
		exp_from_now: Option<u64>,

		/// Set not-before time in seconds from now.
		#[arg(long)]
		nbf_from_now: Option<u64>,

		/// Claim an issuer.
		#[arg(long)]
		issuer: Option<String>,

		/// Claim an audience.
		#[arg(long)]
		audience: Option<String>,
	},

	/// - Synchronize database with primary (secondary only)
	ResyncDatabase,

	/// - Retrieves the saved original PDU before it has been redacted
	GetRetainedPdu {
		event_id: OwnedEventId,
	},

	/// - Developer test stubs
	#[command(subcommand)]
	#[clap(hide(true))]
	Tester(TesterCommand),
}
