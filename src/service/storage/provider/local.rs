use std::{fs, sync::Arc};

use object_store::local::LocalFileSystem;
use tuwunel_core::{
	Result,
	config::{StorageProvider, StorageProviderLocal},
	debug, debug_info, error, trace,
};

use super::Provider;

#[tracing::instrument(name = "new", level = "info", skip_all, err)]
pub(in super::super) fn new(
	args: &crate::Args<'_>,
	name: &str,
	config: &StorageProviderLocal,
) -> Result<Option<(String, Arc<Provider>)>> {
	// Fail successfully if this provider is disabled by the configuration..
	if config.base_path.is_empty() {
		debug!(?name, "'base_path' is not set. This configuration will be skipped");
		return Ok(None);
	}

	if config.create_if_missing {
		trace!(
			%name,
			path = ?config.base_path,
			"Creating directory on local filesystem if missing...",
		);

		fs::create_dir_all(&config.base_path)?;
	}

	trace!(?name, ?config, "Initializing LocalFS...");

	let provider = LocalFileSystem::new_with_prefix(config.base_path.clone())
		.inspect_err(|e| error!("Failed to configure LocalFS storage client: {e}"))?
		.with_automatic_cleanup(config.delete_empty_directories);

	debug_info!(
		name = %name,
		path = ?config.base_path,
		"Started Local FS storage client.",
	);

	let provider = Provider {
		name: name.to_owned(),
		base_path: None, // LocalFileSystem computes base_path internally
		config: StorageProvider::local(config.clone()),
		startup_check: config.startup_check,
		services: args.services.clone(),
		provider: Box::new(provider),
	};

	Ok(Some((name.to_owned(), Arc::new(provider))))
}
