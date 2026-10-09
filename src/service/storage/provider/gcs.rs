//! Google Cloud Storage storage-provider construction.
//!
//! Configuration and environment values feed the object-store GCS builder.
//! Providers without a bucket are treated as disabled, while enabled providers
//! expose signing through the common interface.
//!
//! Credentials are resolved by the object-store builder. With no service
//! account key, application-credentials file or bearer token configured it
//! falls back to the instance metadata server, which is what makes this usable
//! from GKE Workload Identity without any exported key material. URL signing
//! follows the same rule: lacking a private key it uses the IAM `signBlob` API,
//! where Google holds the signing key.

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

/// Builds an enabled Google Cloud Storage provider.
///
/// A configuration without a bucket returns `None`. Other settings override the
/// environment-derived builder before the client and its URL signer are
/// retained by the provider.
#[tracing::instrument(name = "new", level = "info", skip_all, err)]
pub(in super::super) fn new(
	args: &crate::Args<'_>,
	name: &str,
	config: &StorageProviderGcs,
) -> Result<Option<(String, Arc<Provider>)>> {
	// Fail successfully if this provider is disabled by the configuration..
	if config.bucket.is_none() {
		debug!(?name, "gcs_provider.bucket not set. This configuration will be skipped");
		return Ok(None);
	}

	// Seeded from the environment first so GOOGLE_* variables and the instance
	// metadata server keep working; explicit settings below take precedence.
	let mut builder = GoogleCloudStorageBuilder::from_env().with_client_options(
		ClientOptions::new()
			.with_user_agent(user_agent().try_into()?)
			.with_pool_max_idle_per_host(args.server.config.request_idle_per_host.into())
			.with_pool_idle_timeout(Duration::from_secs(args.server.config.request_idle_timeout)),
	);

	if let Some(bucket) = config.bucket.clone() {
		builder = builder.with_bucket_name(bucket);
	}

	// Deliberately not `with_url`: in this crate version a `gs://bucket/path`
	// URL selects the bucket but does not apply the path as an object prefix.
	// Tuwunel's own `base_path` owns prefixing (see Provider::to_abs_path), so
	// accepting a path here would silently drop it.

	if let Some(service_account_path) = config.service_account_path.clone() {
		builder = builder.with_service_account_path(service_account_path);
	}

	if let Some(application_credentials_path) = config.application_credentials_path.clone() {
		builder = builder.with_application_credentials(application_credentials_path);
	}

	if let Some(skip_signature) = config.use_signatures {
		builder = builder.with_skip_signature(!skip_signature);
	}

	trace!(?name, ?config, "Initializing GCS...");

	let client = builder
		.build()
		.inspect_err(|e| error!("Failed to configure GCS storage client: {e}"))?;

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
