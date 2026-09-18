//! Configured object-storage providers for media and other binary data.
//!
//! The service builds local-filesystem and S3-compatible backends behind one
//! provider API. A local media provider rooted under the database directory is
//! supplied when no explicit `media` provider is configured.

/// Backend implementations and the common provider interface.
///
/// Provider instances normalize paths, transfers, and optional URL signing
/// across local and S3-compatible object stores.
/// The module also owns backend construction and startup connectivity checks.
pub mod provider;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use derive_more::Debug;
use futures::TryStreamExt;
/// Object-store transfer types used by provider APIs.
///
/// Re-exporting these types keeps callers independent of the service's direct
/// object-store dependency path. Their behavior is supplied by the selected
/// storage backend.
pub use object_store::{CopyMode, GetResult, GetResultPayload, PutPayload, PutResult};
use tuwunel_core::{
	Result, at,
	config::{StorageProvider, StorageProviderLocal},
	err, implement,
	utils::{BoolExt, stream::IterStream},
};

/// A configured object-storage provider.
///
/// Each provider wraps one local or S3-compatible backend and applies its
/// configured path and transfer policies.
/// Callers share the provider through the service registry's [`Arc`] values.
pub use self::provider::Provider;

/// Registry of configured object-storage providers.
///
/// The registry includes every enabled provider and supplies a default local
/// `media` provider when the configuration does not define one.
/// Provider identifiers determine lookup and iteration order.
#[derive(Debug)]
pub struct Service {
	providers: Providers,

	#[debug(skip)]
	services: Arc<crate::services::OnceServices>,
}

type Providers = BTreeMap<String, Arc<Provider>>;

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			providers: Self::build_providers(args)?,
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.start_providers().await?;

		Ok(())
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
#[tracing::instrument(
	level = "info",
	err(level = "error")
	skip_all,
)]
fn build_providers(args: &crate::Args<'_>) -> Result<Providers> {
	let default_media_provider = args
		.server
		.config
		.storage_provider
		.contains_key("media")
		.is_false()
		.then(|| {
			let db_path = args.server.config.database_path.clone();
			let provider = StorageProviderLocal {
				create_if_missing: true,
				base_path: [db_path, "media".into()]
					.into_iter()
					.collect::<PathBuf>()
					.to_string_lossy()
					.into(),

				..Default::default()
			};

			("media".into(), StorageProvider::local(provider))
		});

	args.server
		.config
		.storage_provider
		.iter()
		.chain(
			default_media_provider
				.iter()
				.map(|(name, conf)| (name, conf)),
		)
		.filter_map(|(name, conf)| match conf {
			| StorageProvider::local(conf) => provider::local::new(args, name, conf).transpose(),
			| StorageProvider::s3(conf) => provider::s3::new(args, name, conf).transpose(),
			| _ => None,
		})
		.collect::<Result<_>>()
}

#[implement(Service)]
async fn start_providers(&self) -> Result {
	self.providers
		.iter()
		.map(at!(1))
		.try_stream()
		.and_then(Provider::start)
		.try_collect()
		.await
}

/// Returns the storage provider with the exact identifier `id`.
///
/// A missing or disabled provider produces a not-found request error. The
/// returned provider remains owned by this service.
#[implement(Service)]
pub fn provider<'a>(&'a self, id: &'a str) -> Result<&'a Arc<Provider>> {
	self.providers
		.get(id)
		.ok_or_else(|| err!(Request(NotFound(error!("No instance of provider")))))
}

/// Returns the first provider configuration whose identifier starts with `id`.
///
/// This uses the same prefix filter as [`Self::configs`], so callers requiring
/// an exact match should validate the returned identifier separately.
#[implement(Service)]
pub fn config<'a>(&'a self, id: &'a str) -> Result<&'a StorageProvider> {
	self.configs(Some(id))
		.next()
		.map(at!(1))
		.ok_or_else(|| err!(Request(NotFound("No configuration for provider"))))
}

/// Iterates over the enabled storage-provider instances.
///
/// Iteration follows the registry's identifier order. Each item remains owned
/// by this service and can be shared by cloning its [`Arc`].
#[implement(Service)]
pub fn providers(&self) -> impl Iterator<Item = &Arc<Provider>> + Send + '_ {
	self.providers.values()
}

/// Iterates over configured providers, optionally filtered by identifier prefix.
///
/// Passing `None` yields every configuration, while `Some(id)` yields entries
/// whose identifiers start with `id`. Disabled configurations can appear here
/// even though they have no corresponding [`Provider`] instance.
#[implement(Service)]
pub fn configs<'a, Id>(
	&'a self,
	id: Id,
) -> impl Iterator<Item = (&'a String, &'a StorageProvider)> + Send + 'a
where
	Id: Into<Option<&'a str>>,
{
	let id = id.into();

	self.services
		.config
		.storage_provider
		.iter()
		.filter(move |(id_, _)| id.is_none_or(|id| id_.starts_with(id)))
}
