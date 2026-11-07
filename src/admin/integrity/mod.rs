mod check;
mod list;

use clap::Subcommand;
use tuwunel_core::Result;

use crate::admin_command_dispatch;

const DEFAULT_LIMIT: usize = 8;

#[admin_command_dispatch(handler_prefix = "integrity")]
#[derive(Debug, Subcommand)]
pub(crate) enum IntegrityCommand {
	/// - List the database integrity checks
	List,

	/// - Run database integrity checks, changing nothing
	Check {
		/// The checks to run; every check that can run here when none is named.
		names: Vec<String>,

		/// The number of findings to show from each check.
		#[arg(long, default_value_t = DEFAULT_LIMIT)]
		limit: usize,
	},
}
