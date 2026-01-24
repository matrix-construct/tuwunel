mod commands;

use clap::Subcommand;
use tuwunel_core::Result;

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(crate) enum TokenCommand {
	/// - Issue a new registration token
	Issue {
		/// The maximum number of times this token is allowed to be used before
		/// it expires.
		#[arg(long)]
		max_uses: Option<u64>,

		/// The maximum age of this token (e.g. 30s, 5m, 7d). It will expire
		/// after this much time has passed.
		#[arg(long)]
		max_age: Option<String>,

		/// A shortcut for `--max-uses 1`.
		#[arg(long)]
		once: bool,
	},

	/// - Revoke a registration token
	Revoke {
		/// The token to revoke.
		token: String,
	},

	/// - List all registration tokens
	List,
}
