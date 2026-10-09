//! Google Cloud Storage storage-provider construction.
//!
//! Configuration and environment values feed the object-store GCS builder.
//! Providers without a bucket are disabled, while enabled providers expose
//! signing through the common interface. Without configured credentials, the
//! builder supports GKE Workload Identity through the instance metadata server.
//! URL signing can use the IAM `signBlob` API when no private key is available.

use std::{sync::Arc, time::Duration};

/// Object-store transfer types used by the GCS provider boundary.
///
/// These re-exports match the common storage module's transfer vocabulary and
/// avoid exposing backend-specific paths to callers.
/// Their behavior remains defined by the object-store backend.
pub use object_store::{GetResult, GetResultPayload, PutPayload, PutResult};
use object_store::{client::ClientOptions, gcp::GoogleCloudStorageBuilder, signer::Signer};
use tuwunel_core::{
	Result,
	config::{StorageProvider, StorageProviderGcs},
	debug, debug_info, error, trace,
	version::user_agent,
};

use super::Provider;

type Registration = (String, Arc<Provider>);

/// Builds an enabled Google Cloud Storage provider.
///
/// A configuration without a bucket returns `None`. Other settings are applied
/// to the environment-derived builder before retaining the client and signer.
#[tracing::instrument(name = "new", level = "info", skip_all, err)]
pub(in super::super) fn new(
	args: &crate::Args<'_>,
	name: &str,
	config: &StorageProviderGcs,
) -> Result<Option<Registration>> {
	let Some(bucket) = config.bucket.as_deref() else {
		debug!(?name, "gcs_provider.bucket not set. This configuration will be skipped");
		return Ok(None);
	};

	// Seed from the environment so GOOGLE_* variables and the instance metadata
	// server remain available alongside configured file paths.
	let options = ClientOptions::new()
		.with_user_agent(user_agent().try_into()?)
		.with_pool_max_idle_per_host(args.server.config.request_idle_per_host.into())
		.with_pool_idle_timeout(Duration::from_secs(args.server.config.request_idle_timeout));

	let builder = GoogleCloudStorageBuilder::from_env()
		.with_client_options(options)
		.with_bucket_name(bucket);

	// `with_url` discards its object prefix; `base_path` owns prefixing.
	let builder = match config.service_account_path.as_deref() {
		| Some(path) => builder.with_service_account_path(path),
		| None => builder,
	};

	let builder = match config.application_credentials_path.as_deref() {
		| Some(path) => builder.with_application_credentials(path),
		| None => builder,
	};

	let builder = match config.use_signatures {
		| Some(use_signatures) => builder.with_skip_signature(!use_signatures),
		| None => builder,
	};

	trace!(?name, ?config, "Initializing GCS...");

	let client = builder
		.build()
		.inspect_err(|e| error!(%e, "Failed to configure GCS storage client"))?;

	debug_info!(name = %name, "Started GCS storage client.");

	#[allow(clippy::allow_attributes, clippy::redundant_clone)] // buggy, nursery
	let signer: Arc<dyn Signer> = Arc::new(client.clone());

	let provider = Provider {
		name: name.to_owned(),
		base_path: config.base_path.clone().map(Into::into),
		config: StorageProvider::gcs(Box::new(config.clone())),
		startup_check: config.startup_check,
		services: args.services.clone(),
		provider: Box::new(client),
		signer: Some(signer),
	};

	Ok(Some((name.to_owned(), Arc::new(provider))))
}
