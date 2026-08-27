use tuwunel_core::{Result, result::NotFound};

use super::conduit::migrate_conduit_media;
use crate::{
	Services,
	media::migrations::{checkup_sha256_media, migrate_sha256_media},
};

/// Imports a Conduit database's content-addressed media into tuwunel's
/// key-addressed store when it is present and not yet imported.
///
/// Otherwise runs the key-addressed media migrations.
pub(super) async fn migrate_media(services: &Services) -> Result {
	services.server.check_running()?;

	let db = &services.db;
	let config = &services.server.config;

	let sha256_done = !db["global"]
		.get(b"feat_sha256_media")
		.await
		.is_not_found();

	// The foreign CF persists, so the marker (not its presence) is the latch.
	if !sha256_done
		&& db
			.open_cf("servernamemediaid_metadata")?
			.is_some()
	{
		migrate_conduit_media(services).await?;
		db["global"].insert(b"feat_sha256_media", []);
		return Ok(());
	}

	if !sha256_done {
		migrate_sha256_media(services).await?;
	} else if config.media_startup_check {
		checkup_sha256_media(services).await?;
	}

	Ok(())
}
