use tuwunel_core::Result;

use super::{conduit::migrate_conduit_knocks, pending};
use crate::Services;

/// Imports a Conduit database's pending knocks once.
///
/// Gated on its own marker and the source column's presence, it runs only for a
/// Conduit database and
/// only the first time; a re-import would resurrect a knock the user later
/// resolved.
pub(super) async fn import_conduit_knocks(services: &Services) -> Result {
	services.server.check_running()?;

	let db = &services.db;

	if db.open_cf("roomuserid_knockcount")?.is_some()
		&& pending(services, "imported_conduit_knocks").await?
	{
		migrate_conduit_knocks(services).await?;
		db["global"].insert("imported_conduit_knocks", []);
	}

	Ok(())
}
