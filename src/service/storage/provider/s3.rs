use std::{sync::Arc, time::Duration};

pub use object_store::{GetResult, GetResultPayload, PutPayload, PutResult};
use object_store::{aws::AmazonS3Builder, client::ClientOptions};
use tuwunel_core::{
	Result,
	config::{StorageProvider, StorageProviderS3},
	debug, debug_info, error, trace,
	version::user_agent,
};

use super::Provider;

#[tracing::instrument(name = "new", level = "info", skip_all, err)]
pub(in super::super) fn new(
	args: &crate::Args<'_>,
	name: &str,
	config: &StorageProviderS3,
) -> Result<Option<(String, Arc<Provider>)>> {
	// Fail successfully if this provider is disabled by the configuration..
	if config.url.is_none() && config.bucket.is_none() {
		debug!(?name, "s3_provider.bucket not set. This configuration will be skipped");
		return Ok(None);
	}

	let mut builder = AmazonS3Builder::from_env().with_client_options(
		ClientOptions::new()
			.with_user_agent(user_agent().try_into()?)
			.with_pool_max_idle_per_host(args.server.config.request_idle_per_host.into())
			.with_pool_idle_timeout(Duration::from_secs(args.server.config.request_idle_timeout)),
	);

	if let Some(url) = config.url.clone() {
		builder = builder.with_url(url);
	}

	if let Some(region) = config.region.clone() {
		builder = builder.with_region(region);
	}

	if let Some(bucket) = config.bucket.clone() {
		builder = builder.with_bucket_name(bucket);
	}

	if let Some(key) = config.key.clone() {
		builder = builder.with_access_key_id(key);
	}

	if let Some(secret) = config.secret.clone() {
		builder = builder.with_secret_access_key(secret);
	}

	if let Some(kms) = config.kms.clone() {
		builder = builder.with_ssec_encryption(kms);
	}

	if let Some(token) = config.token.clone() {
		builder = builder.with_token(token);
	}

	if let Some(endpoint) = config.endpoint.clone() {
		builder = builder.with_endpoint(endpoint);
	}

	if let Some(use_bucket_key) = config.use_bucket_key {
		builder = builder.with_bucket_key(use_bucket_key);
	}

	if let Some(use_https) = config.use_https {
		builder = builder.with_allow_http(!use_https);
	}

	if let Some(use_signatures) = config.use_signatures {
		builder = builder.with_skip_signature(!use_signatures);
	}

	if let Some(use_payload_signatures) = config.use_payload_signatures {
		builder = builder.with_unsigned_payload(!use_payload_signatures);
	}

	if let Some(use_vhost_request) = config.use_vhost_request {
		builder = builder.with_virtual_hosted_style_request(use_vhost_request);
	}

	trace!(?name, ?config, "Initializing S3...");

	let provider = builder
		.build()
		.map(Box::from)
		.inspect_err(|e| error!("Failed to configure S3 storage client: {e}"))?;

	debug_info!(
		credentials = ?provider.credentials(),
		"Started S3 storage client."
	);

	let provider = Provider {
		name: name.to_owned(),
		base_path: config.base_path.clone().map(Into::into),
		config: StorageProvider::s3(config.clone()),
		startup_check: config.startup_check,
		services: args.services.clone(),
		provider,
	};

	Ok(Some((name.to_owned(), Arc::new(provider))))
}
