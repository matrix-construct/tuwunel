use tuwunel_core::{Result, result::NotFound};

use super::conduit::migrate_conduit_knocks;
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

	let pending = db["global"]
		.get(b"imported_conduit_knocks")
		.await
		.is_not_found();

	if pending && db.open_cf("roomuserid_knockcount")?.is_some() {
		migrate_conduit_knocks(services).await?;
		db["global"].insert(b"imported_conduit_knocks", []);
	}

	Ok(())
}
