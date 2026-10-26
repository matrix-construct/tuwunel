use tuwunel_core::Result;

use super::{conduit::migrate_conduit_media, marker_present};
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
	let progress = &services.server.progress;

	let sha256_done = marker_present(services, "feat_sha256_media").await?;

	// The foreign CF persists, so the marker (not its presence) is the latch.
	if !sha256_done
		&& db
			.open_cf("servernamemediaid_metadata")?
			.is_some()
	{
		progress.begin("migrate_conduit_media");
		migrate_conduit_media(services).await?;
		db["global"].insert("feat_sha256_media", []);
		return Ok(());
	}

	match () {
		| () if !sha256_done => {
			progress.begin("migrate_sha256_media");
			migrate_sha256_media(services).await?;
		},
		| () if config.media.startup_check => {
			progress.begin("checkup_sha256_media");
			checkup_sha256_media(services).await?;
		},
		| () => {},
	}

	Ok(())
}
